// Package greet builds greetings. It pulls in a few third-party modules to
// exercise go_mod_binary: golang.org/x/text (large), golang.org/x/crypto
// (Go assembly) and github.com/BurntSushi/toml (upper-case module path).
package greet

import (
	"encoding/hex"
	"fmt"

	"github.com/BurntSushi/toml"
	"golang.org/x/crypto/blake2b"
	"golang.org/x/text/cases"
	"golang.org/x/text/language"
)

// Config is the greeting configuration.
type Config struct {
	Greeting string `toml:"greeting"`
}

// Greet returns a greeting for name using the TOML config.
func Greet(configTOML, name string) (string, error) {
	var cfg Config
	if _, err := toml.Decode(configTOML, &cfg); err != nil {
		return "", err
	}
	title := cases.Title(language.English).String(name)
	sum := blake2b.Sum256([]byte(title))
	return fmt.Sprintf("%s, %s! (%s)", cfg.Greeting, title, hex.EncodeToString(sum[:4])), nil
}
