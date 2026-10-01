// Shared by the client bundle and the server.
export interface Greeting {
  name: string;
  excited?: boolean;
}

export function greet({ name, excited = false }: Greeting): string {
  return `Hello, ${name}${excited ? "!" : "."}`;
}
