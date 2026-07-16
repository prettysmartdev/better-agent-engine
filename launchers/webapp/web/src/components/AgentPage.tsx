import { useState, type FormEvent } from "react";
import { ApiError, setAuthToken, triggerAgent } from "../api/client";
import type { Agent } from "../api/types";
import AgentIcon from "./AgentIcon";

type ChatMessage = {
  id: number;
  sender: "operator" | "agent";
  text: string;
};

interface AgentPageProps {
  agent: Agent;
}

export default function AgentPage({ agent }: AgentPageProps) {
  const [input, setInput] = useState("");
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Shown after a 401: the deployment set BAE_LAUNCHER_API_TOKEN, so triggers
  // need the operator to enter it. Held in memory only (clears on refresh).
  const [needsToken, setNeedsToken] = useState(false);
  const [tokenInput, setTokenInput] = useState("");

  const send = async (text: string) => {
    if (sending || !text.trim()) return;
    const userMessage: ChatMessage = {
      id: Date.now(),
      sender: "operator",
      text,
    };
    const responseId = userMessage.id + 1;
    setMessages((current) => [
      ...current,
      userMessage,
      { id: responseId, sender: "agent", text: "" },
    ]);
    setInput("");
    setSending(true);
    setError(null);

    try {
      await triggerAgent(agent.name, agent.chat_input_field, text, (chunk) => {
        setMessages((current) =>
          current.map((message) =>
            message.id === responseId
              ? { ...message, text: message.text + chunk }
              : message,
          ),
        );
      });
    } catch (caught) {
      if (caught instanceof ApiError && caught.status === 401) {
        setNeedsToken(true);
        setError(
          "This launcher requires an API token (BAE_LAUNCHER_API_TOKEN). " +
            "Enter it below, then send your message again.",
        );
      } else {
        const message =
          caught instanceof Error
            ? caught.message
            : "Could not trigger this agent.";
        setError(message);
      }
      setMessages((current) =>
        current.map((item) =>
          item.id === responseId
            ? { ...item, text: item.text || "The agent could not be started." }
            : item,
        ),
      );
    } finally {
      setSending(false);
    }
  };

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    void send(input);
  };

  return (
    <section className="chat-page">
      <a className="back-link" href="/">
        ← All agents
      </a>
      <header className="agent-header">
        <AgentIcon icon={agent.icon} large />
        <div>
          <p className="eyebrow">Agent</p>
          <h1>{agent.display_name ?? agent.name}</h1>
          {agent.description && <p>{agent.description}</p>}
        </div>
      </header>

      <div
        className="chat-transcript"
        aria-live="polite"
        aria-label="Chat transcript"
      >
        {messages.length === 0 ? (
          <p className="chat-placeholder">
            Send a message to start a new conversation. This transcript exists
            only in this browser tab and clears on refresh.
          </p>
        ) : (
          messages.map((message) => (
            <div
              className={`chat-message chat-message-${message.sender}`}
              key={message.id}
            >
              <span className="message-label">
                {message.sender === "operator"
                  ? "You"
                  : (agent.display_name ?? agent.name)}
              </span>
              <pre>{message.text || "…"}</pre>
            </div>
          ))
        )}
      </div>

      {error && <p className="error-banner">{error}</p>}

      {needsToken && (
        <form
          className="token-form"
          onSubmit={(event) => {
            event.preventDefault();
            setAuthToken(tokenInput);
            setTokenInput("");
            setNeedsToken(false);
            setError(null);
          }}
        >
          <label className="sr-only" htmlFor="api-token-input">
            API token
          </label>
          <input
            autoComplete="off"
            id="api-token-input"
            onChange={(event) => setTokenInput(event.target.value)}
            placeholder="API token"
            type="password"
            value={tokenInput}
          />
          <button disabled={!tokenInput.trim()} type="submit">
            Use token
          </button>
          <p className="token-note">
            Kept in memory only — a page refresh clears it.
          </p>
        </form>
      )}

      {agent.prompts.length > 0 && (
        <div className="prompt-row" aria-label="Suggested prompts">
          {agent.prompts.map((prompt) => (
            <button
              className="prompt-button"
              disabled={sending}
              key={`${prompt.label}-${prompt.prompt}`}
              onClick={() => void send(prompt.prompt)}
              type="button"
            >
              {prompt.label}
            </button>
          ))}
        </div>
      )}

      <form className="chat-composer" onSubmit={submit}>
        <label className="sr-only" htmlFor="chat-input">
          Message for {agent.display_name ?? agent.name}
        </label>
        <textarea
          id="chat-input"
          onChange={(event) => setInput(event.target.value)}
          placeholder={`Message ${agent.display_name ?? agent.name}`}
          rows={3}
          value={input}
        />
        <button
          className="send-button"
          disabled={sending || !input.trim()}
          type="submit"
        >
          {sending ? "Sending…" : "Send"}
        </button>
      </form>
    </section>
  );
}
