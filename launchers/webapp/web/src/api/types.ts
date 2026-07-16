export interface Prompt {
  label: string;
  prompt: string;
}

/** The deliberately safe, browser-facing shape from `baeapi` introspection. */
export interface Agent {
  name: string;
  display_name: string | null;
  description: string | null;
  icon: string | null;
  request_schema: unknown | null;
  chat_input_field: string;
  prompts: Prompt[];
}
