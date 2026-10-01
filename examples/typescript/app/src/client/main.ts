import { greet } from "../shared/greet.js";

const heading = document.getElementById("greeting") as HTMLHeadingElement;
heading.textContent = greet({ name: "browser", excited: true });

const server = document.getElementById("server") as HTMLParagraphElement;
fetch("/api/hello/server")
  .then(res => res.json() as Promise<{ message: string }>)
  .then(({ message }) => {
    server.textContent = message;
  });
