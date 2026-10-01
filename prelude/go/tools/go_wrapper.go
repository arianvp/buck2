/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

package main

import (
	"bufio"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"sort"
	"strings"
)

func loadArgs(args []string) []string {
	newArgs := make([]string, 0, 0)
	for _, arg := range args {
		if !strings.HasPrefix(arg, "@") {
			newArgs = append(newArgs, arg)
		} else {
			file, _ := os.Open(arg[1:])
			defer file.Close()
			scanner := bufio.NewScanner(file)
			for scanner.Scan() {
				newArgs = append(newArgs, scanner.Text())
			}
		}
	}
	return newArgs
}

// isPathByte reports whether c can be part of a path name (as opposed to,
// say, the space or quote before a path in an error message).
func isPathByte(c byte) bool {
	return c >= 0x80 || c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' ||
		strings.IndexByte(`/\.-_@!~+`, c) >= 0
}

// trimDir makes the paths under dir in s relative to dir: s == dir becomes
// ".", and "<dir>/" is removed wherever a path starts with it (paths also
// appear inside error messages).
func trimDir(s, dir string) string {
	if s == dir {
		return "."
	}
	prefix := dir + string(filepath.Separator)
	var b strings.Builder
	for {
		i := strings.Index(s, prefix)
		if i < 0 {
			break
		}
		if i == 0 || !isPathByte(s[i-1]) {
			b.WriteString(s[:i])
		} else {
			// In the middle of some other path: keep it.
			b.WriteString(s[:i+len(prefix)])
		}
		s = s[i+len(prefix):]
	}
	b.WriteString(s)
	return b.String()
}

// trimDirs applies trimDir to every string in a decoded JSON value.
func trimDirs(v any, dirs []string) any {
	switch v := v.(type) {
	case string:
		for _, dir := range dirs {
			v = trimDir(v, dir)
		}
		return v
	case []any:
		for i, e := range v {
			v[i] = trimDirs(e, dirs)
		}
		return v
	case map[string]any:
		for k, e := range v {
			v[k] = trimDirs(e, dirs)
		}
		return v
	default:
		return v
	}
}

func jsonStreamToArray(r io.Reader, w io.Writer, trim []string) error {
	var objs []any
	for dec := json.NewDecoder(r); dec.More(); {
		var obj any
		if err := dec.Decode(&obj); err != nil {
			return fmt.Errorf("failed to decode json: %w", err)
		}
		if len(trim) > 0 {
			obj = trimDirs(obj, trim)
		}
		objs = append(objs, obj)
	}
	if err := json.NewEncoder(w).Encode(objs); err != nil {
		return fmt.Errorf("failed to encode json: %w", err)
	}
	return nil
}

func main() {
	os.Args = loadArgs(os.Args)
	var wrappedBinary = flag.String("go", "", "wrapped go binary")
	var goRoot = flag.String("goroot", "", "go root")
	var defaultGoOS = flag.String("default-goos", "", "default GOOS (if not set by env)")
	var defaultGoArch = flag.String("default-goarch", "", "default GOARCH (if not set by env)")
	var outputFile = flag.String("output", "", "file to redirect stdout to")
	var convertJsonStream = flag.Bool("convert-json-stream", false, "convert json stream to array")
	var trimCwd = flag.Bool("trim-cwd", false, "with -convert-json-stream: make paths under the working directory relative to it, so that the output does not depend on where the action ran")
	var gnuBuildID = flag.Bool("gnu-build-id", false, "rewrite .note.gnu.build-id with sha256 of the linked output (internal Go links only)")
	var pruneModCache = flag.Bool("prune-mod-cache", false, "after a successful run, delete what loading packages does not need from $GOMODCACHE: the module zips (the extracted modules are kept), VCS checkouts and checksum database records")
	var cleanEnv = flag.Bool("clean-env", false, "run the command with only the environment variables named by -keep-env, plus GOCACHE, GOPATH, HOME and TMPDIR in $BUCK_SCRATCH_PATH (buck2 runs local actions in its own environment, which remote actions never see)")
	var keepEnv = flag.String("keep-env", "", "with -clean-env: comma-separated names of the environment variables to keep")
	var checkModCacheSums = flag.String("check-mod-cache-sums", "", "after a successful run, check the modules and go.mod files in $GOMODCACHE against this go.sum file, and fail if it lacks or contradicts any of their checksums")
	flag.Parse()
	unknownArgs := flag.Args()

	if *wrappedBinary == "" {
		log.Fatal("No wrapped binary specified")
	}
	if *trimCwd && !*convertJsonStream {
		log.Fatal("-trim-cwd requires -convert-json-stream")
	}
	if *keepEnv != "" && !*cleanEnv {
		log.Fatal("-keep-env requires -clean-env")
	}

	absWrappedBinary, err := filepath.Abs(*wrappedBinary)
	if err != nil {
		log.Fatalf("Failed to resolve wrapped binary: %s", err)
	}

	envs := make(map[string]string)
	for _, e := range os.Environ() {
		pair := strings.SplitN(e, "=", 2)
		envs[pair[0]] = pair[1]
	}
	if *cleanEnv {
		kept := make(map[string]string)
		for _, k := range append(strings.Split(*keepEnv, ","), "BUCK_SCRATCH_PATH") {
			if v, ok := envs[k]; ok {
				kept[k] = v
			}
		}
		envs = kept
	}

	goroot := *goRoot
	if goroot == "" {
		goroot = envs["GOROOT"]
	}

	if envs["GOOS"] == "" && *defaultGoOS != "" {
		envs["GOOS"] = *defaultGoOS
	}
	if envs["GOARCH"] == "" && *defaultGoArch != "" {
		envs["GOARCH"] = *defaultGoArch
	}

	if goroot != "" {
		absGoroot, err := filepath.Abs(goroot)
		if err != nil {
			log.Fatalf("Failed to resolve GOROOT: %s", err)
		}
		envs["GOROOT"] = absGoroot
	}

	if buckScratchPath, ok := envs["BUCK_SCRATCH_PATH"]; ok {
		absBuckScratchPath, err := filepath.Abs(buckScratchPath)
		if err != nil {
			log.Fatalf("Failed to resolve BUCK_SCRATCH_PATH: %s", err)
		}
		envs["GOCACHE"] = absBuckScratchPath
		envs["TMPDIR"] = absBuckScratchPath
		if *cleanEnv {
			// Not the user's: $HOME holds git and netrc configuration,
			// $GOPATH checksum database state.
			envs["HOME"] = filepath.Join(absBuckScratchPath, "home")
			envs["GOPATH"] = filepath.Join(absBuckScratchPath, "gopath")
		}
	} else if *cleanEnv {
		log.Fatal("-clean-env requires BUCK_SCRATCH_PATH")
	}

	cwd, err := os.Getwd()
	if err != nil {
		log.Fatalf("Failed to get current working directory: %s", err)
	}

	for i, arg := range unknownArgs {
		unknownArgs[i] = strings.ReplaceAll(arg, "%cwd%", cwd)
	}

	// Some Go env vars (e.g. GOMODCACHE) must be absolute paths.
	for k, v := range envs {
		envs[k] = strings.ReplaceAll(v, "%cwd%", cwd)
	}

	var output *os.File
	if *outputFile == "" {
		output = os.Stdout
	} else {
		output, err = os.Create(*outputFile)
		if err != nil {
			log.Fatalf("Error creating output file: %s", err)
			os.Exit(1)
		}
		defer output.Close()
	}

	cmd := exec.Command(absWrappedBinary, unknownArgs...)

	cmd.Env = make([]string, 0, len(envs)/2)
	for k, v := range envs {
		cmd.Env = append(cmd.Env, k+"="+v)
	}

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		log.Fatalf("Error creating stdout pipe: %s", err)
	}
	defer stdout.Close()

	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		log.Fatalf("Error starting command: %s", err)
	}

	if *convertJsonStream {
		var trim []string
		if *trimCwd {
			// `go -C dir` resolves symlinks in the working directory, while
			// paths we pass (GOROOT, GOMODCACHE) keep them: trim both forms.
			trim = append(trim, cwd)
			if real, err := filepath.EvalSymlinks(cwd); err == nil && real != cwd {
				trim = append(trim, real)
				// Longest first, in case one is inside the other.
				sort.Slice(trim, func(i, j int) bool { return len(trim[i]) > len(trim[j]) })
			}
		}
		if err := jsonStreamToArray(stdout, output, trim); err != nil {
			log.Fatalf("Error converting json stream: %s", err)
		}
	} else {
		if _, err := io.Copy(output, stdout); err != nil {
			log.Fatalf("Error copying stdout: %s", err)
		}
	}

	err = cmd.Wait()
	if err != nil {
		exitCode := 1
		if exitErr, ok := err.(*exec.ExitError); ok {
			exitCode = exitErr.ExitCode()
		}
		fmt.Fprintln(os.Stderr, "Error running command:", err)
		os.Exit(exitCode)
	}

	if *gnuBuildID {
		out := outputPath(unknownArgs)
		if out == "" {
			log.Fatalf("--gnu-build-id set but no -o output found in linker args: %v", unknownArgs)
		}
		if err := setGNUBuildID(out); err != nil {
			log.Fatalf("Error setting GNU build id: %s", err)
		}
	}

	if *checkModCacheSums != "" {
		if err := checkModuleCacheSums(envs["GOMODCACHE"], *checkModCacheSums); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	}

	if *pruneModCache {
		if err := pruneModuleCache(envs["GOMODCACHE"]); err != nil {
			log.Fatalf("Error pruning the module cache: %s", err)
		}
	}
}

// pruneModuleCache deletes from a module cache what loading packages does
// not need:
//   - the module zips (cache/download/<module>/@v/<version>.zip). Loading
//     packages only needs the extracted module directories, the .mod files
//     and the .ziphash files; `go` compares the .ziphash files with go.sum
//     instead of rehashing the zips.
//   - VCS checkouts (cache/vcs), left by direct fetches from version control.
//   - checksum database records (cache/download/sumdb).
func pruneModuleCache(modCache string) error {
	if modCache == "" {
		return fmt.Errorf("GOMODCACHE is not set")
	}
	// `go mod download` doesn't create the module cache when there is
	// nothing to download (a module with no requirements), but it is the
	// action's output.
	if err := os.MkdirAll(modCache, 0o755); err != nil {
		return err
	}
	for _, dir := range []string{filepath.Join("cache", "vcs"), filepath.Join("cache", "download", "sumdb")} {
		if err := os.RemoveAll(filepath.Join(modCache, dir)); err != nil {
			return err
		}
	}
	root := filepath.Join(modCache, "cache", "download")
	return filepath.WalkDir(root, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			if path == root && errors.Is(err, fs.ErrNotExist) {
				// Nothing was downloaded.
				return nil
			}
			return err
		}
		if d.Type().IsRegular() && strings.HasSuffix(d.Name(), ".zip") && filepath.Base(filepath.Dir(path)) == "@v" {
			return os.Remove(path)
		}
		return nil
	})
}

// checkModuleCacheSums checks the checksum of every module in a module cache
// (its .ziphash file: the hash of the module zip, which go checked the
// extracted module against) and of every go.mod file in it
// (cache/download/<module>/@v/<version>.mod) against a go.sum file. With
// GOSUMDB=off, go accepts modules and go.mod files that go.sum does not list:
// this makes go.sum the only source of checksums.
func checkModuleCacheSums(modCache, goSum string) error {
	if modCache == "" {
		return fmt.Errorf("GOMODCACHE is not set")
	}
	data, err := os.ReadFile(goSum)
	if err != nil {
		return err
	}
	// "<module> <version>" or "<module> <version>/go.mod" -> hashes
	sums := make(map[string][]string)
	for _, line := range strings.Split(string(data), "\n") {
		if f := strings.Fields(line); len(f) == 3 {
			key := f[0] + " " + f[1]
			sums[key] = append(sums[key], f[2])
		}
	}

	var missing, mismatched []string
	root := filepath.Join(modCache, "cache", "download")
	err = filepath.WalkDir(root, func(path string, d os.DirEntry, err error) error {
		if err != nil {
			if path == root && errors.Is(err, fs.ErrNotExist) {
				// Nothing was downloaded.
				return nil
			}
			return err
		}
		if !d.Type().IsRegular() || filepath.Base(filepath.Dir(path)) != "@v" {
			return nil
		}
		var version, hash string
		if v, ok := strings.CutSuffix(d.Name(), ".ziphash"); ok {
			data, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			version, hash = v, strings.TrimSpace(string(data))
		} else if v, ok := strings.CutSuffix(d.Name(), ".mod"); ok {
			data, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			version, hash = v+"/go.mod", goModHash(data)
		} else {
			return nil
		}
		modDir, err := filepath.Rel(root, filepath.Dir(filepath.Dir(path)))
		if err != nil {
			return err
		}
		key := unescapeModulePath(filepath.ToSlash(modDir)) + " " + unescapeModulePath(version)
		if want, ok := sums[key]; !ok {
			missing = append(missing, key)
		} else if !slices.Contains(want, hash) {
			mismatched = append(mismatched, fmt.Sprintf("%s: %s in the module cache, %s in go.sum", key, hash, strings.Join(want, " ")))
		}
		return nil
	})
	if err != nil {
		return err
	}

	var msgs []string
	if len(missing) > 0 {
		msgs = append(msgs, "go.sum is missing the checksums of these modules and go.mod files, so nothing verified them:\n\t"+strings.Join(missing, "\n\t")+
			"\nRun `go mod tidy`. If go.mod says `go 1.16` or older, `go mod download` also fetches modules that go mod tidy keeps no checksums for: run `go mod tidy -go=1.17`.")
	}
	if len(mismatched) > 0 {
		msgs = append(msgs, "the module cache does not match go.sum:\n\t"+strings.Join(mismatched, "\n\t"))
	}
	if len(msgs) > 0 {
		return errors.New(strings.Join(msgs, "\n"))
	}
	return nil
}

// goModHash returns the go.sum checksum of a go.mod file: the "h1:" hash
// (golang.org/x/mod/sumdb/dirhash.Hash1) of a single file named go.mod.
func goModHash(data []byte) string {
	h := sha256.New()
	fmt.Fprintf(h, "%x  %s\n", sha256.Sum256(data), "go.mod")
	return "h1:" + base64.StdEncoding.EncodeToString(h.Sum(nil))
}

// unescapeModulePath undoes the module cache's escaping of upper-case letters
// in module paths and versions ("!" and the lower-case letter, see
// golang.org/x/mod/module.EscapePath).
func unescapeModulePath(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		if s[i] == '!' && i+1 < len(s) && 'a' <= s[i+1] && s[i+1] <= 'z' {
			b.WriteByte(s[i+1] - 'a' + 'A')
			i++
		} else {
			b.WriteByte(s[i])
		}
	}
	return b.String()
}
