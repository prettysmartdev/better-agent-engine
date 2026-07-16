interface AgentIconProps {
  icon: string | null;
  large?: boolean;
}

/** Presentation icons may be emoji or a configured image URL. */
export default function AgentIcon({ icon, large = false }: AgentIconProps) {
  const imageUrl = icon && /^(https?:)?\/\//.test(icon) ? icon : null;
  const className = `agent-icon${large ? " agent-icon-large" : ""}`;

  return (
    <span className={className} aria-hidden="true">
      {imageUrl ? <img alt="" src={imageUrl} /> : (icon ?? "✦")}
    </span>
  );
}
