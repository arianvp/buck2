package main

import (
	_ "embed"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"

	"example.com/gomodapp/internal/greet"
	"github.com/go-chi/chi/v5"
	"github.com/google/uuid"
)

//go:embed static/config.toml
var config string

func handler(w http.ResponseWriter, r *http.Request) {
	msg, err := greet.Greet(config, chi.URLParam(r, "name"))
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	w.Header().Set("X-Request-Id", uuid.NewString())
	fmt.Fprintln(w, msg)
}

func main() {
	once := flag.String("greet", "", "print a greeting for this name and exit")
	flag.Parse()

	if *once != "" {
		msg, err := greet.Greet(config, *once)
		if err != nil {
			log.Fatal(err)
		}
		fmt.Println(msg)
		return
	}

	r := chi.NewRouter()
	r.Get("/hello/{name}", handler)
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	log.Printf("listening on :%s", port)
	log.Fatal(http.ListenAndServe(":"+port, r))
}
