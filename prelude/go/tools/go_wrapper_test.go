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
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestGoModHash(t *testing.T) {
	// From the go.sum of a module that requires github.com/google/uuid v1.6.0.
	got := goModHash([]byte("module github.com/google/uuid\n"))
	if want := "h1:TIyPZe4MgqvfeYDBFedMoGGpEw/LqOeaOT+nhxU+yHo="; got != want {
		t.Errorf("goModHash = %s, want %s", got, want)
	}
}

func TestUnescapeModulePath(t *testing.T) {
	for in, want := range map[string]string{
		"github.com/!burnt!sushi/toml": "github.com/BurntSushi/toml",
		"golang.org/x/text":            "golang.org/x/text",
		"v1.0.0-!r!c1":                 "v1.0.0-RC1",
	} {
		if got := unescapeModulePath(in); got != want {
			t.Errorf("unescapeModulePath(%q) = %q, want %q", in, got, want)
		}
	}
}

func writeFile(t *testing.T, path, data string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(data), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestCheckModuleCacheSums(t *testing.T) {
	const goMod = "module github.com/BurntSushi/toml\n"
	modHash := goModHash([]byte(goMod))
	cache := t.TempDir()
	v := filepath.Join(cache, "cache", "download", "github.com", "!burnt!sushi", "toml", "@v")
	writeFile(t, filepath.Join(v, "v1.5.0.mod"), goMod)
	writeFile(t, filepath.Join(v, "v1.5.0.ziphash"), "h1:zip\n")
	writeFile(t, filepath.Join(v, "v1.5.0.info"), "{}")
	writeFile(t, filepath.Join(cache, "github.com", "!burnt!sushi", "toml@v1.5.0", "toml.go"), "package toml\n")

	for _, tc := range []struct {
		name  string
		goSum string
		err   string
	}{
		{
			name: "complete",
			goSum: "github.com/BurntSushi/toml v1.5.0 h1:zip\n" +
				"github.com/BurntSushi/toml v1.5.0/go.mod " + modHash + "\n",
		},
		{
			name:  "missing zip hash",
			goSum: "github.com/BurntSushi/toml v1.5.0/go.mod " + modHash + "\n",
			err:   "go.sum is missing the checksums of these modules and go.mod files, so nothing verified them:\n\tgithub.com/BurntSushi/toml v1.5.0\nRun `go mod tidy`.",
		},
		{
			name: "mismatch",
			goSum: "github.com/BurntSushi/toml v1.5.0 h1:other\n" +
				"github.com/BurntSushi/toml v1.5.0/go.mod " + modHash + "\n",
			err: "the module cache does not match go.sum:\n\tgithub.com/BurntSushi/toml v1.5.0: h1:zip in the module cache, h1:other in go.sum",
		},
		{
			name:  "missing both",
			goSum: "",
			err:   "\tgithub.com/BurntSushi/toml v1.5.0/go.mod\n\tgithub.com/BurntSushi/toml v1.5.0",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			goSum := filepath.Join(t.TempDir(), "go.sum")
			writeFile(t, goSum, tc.goSum)
			err := checkModuleCacheSums(cache, goSum)
			if tc.err == "" {
				if err != nil {
					t.Fatalf("unexpected error: %v", err)
				}
				return
			}
			if err == nil || !strings.Contains(err.Error(), tc.err) {
				t.Fatalf("error %q does not contain %q", err, tc.err)
			}
		})
	}

	// An empty module cache (nothing to download) passes.
	goSum := filepath.Join(t.TempDir(), "go.sum")
	writeFile(t, goSum, "")
	if err := checkModuleCacheSums(t.TempDir(), goSum); err != nil {
		t.Errorf("empty module cache: %v", err)
	}
}
