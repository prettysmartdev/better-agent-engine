import type { EventCategory } from "../api/eventTypes";

/**
 * A small color- and shape-coded marker for an event category. Color alone is
 * never the only signal — the shape (and, in the graph, the type label) also
 * distinguishes categories, so the coding survives color-blindness.
 */
export default function ShapeMarker({ category }: { category: EventCategory }) {
  return (
    <span
      className={`shape shape-${category.shape}`}
      style={{ ["--marker-color" as string]: category.color }}
      aria-hidden="true"
    />
  );
}
