import type { Agent } from "../api/types";

/** A minimal, valid agent fixture. Individual fields can be overridden per test. */
export function makeAgent(overrides: Partial<Agent> & { name: string }): Agent {
  return {
    display_name: null,
    description: null,
    icon: null,
    request_schema: null,
    chat_input_field: "prompt",
    prompts: [],
    ...overrides,
  };
}

/** Three-agent fixture set, matching the work item's "3+ agents" test scenarios. */
export function makeAgents(): Agent[] {
  return [
    makeAgent({
      name: "summarize",
      display_name: "Summarizer",
      description: "Summarizes long documents.",
      icon: "📝",
      chat_input_field: "prompt",
      prompts: [
        { label: "Summarize this", prompt: "Summarize the attached text." },
      ],
    }),
    makeAgent({
      name: "translate",
      display_name: "Translator",
      description: "Translates text between languages.",
      icon: "🌐",
      chat_input_field: "text",
      prompts: [{ label: "To French", prompt: "Translate to French." }],
    }),
    makeAgent({
      name: "triage",
      display_name: "Triage",
      description: "Classifies incoming tickets.",
      icon: "🚦",
      chat_input_field: "prompt",
      prompts: [],
    }),
  ];
}
