// Tiny in-process pub/sub for SSE. No EventEmitter import needed — a Set of
// callbacks is sufficient for a single-process server.

export interface BusEvent {
  kind: string;
  actor: string;
  payload: unknown;
  at: string;
}

type Subscriber = (event: BusEvent) => void;

const subscribers = new Set<Subscriber>();

export function publish(event: BusEvent): void {
  for (const fn of subscribers) {
    try {
      fn(event);
    } catch {
      // Don't let a broken subscriber crash the mutation path.
    }
  }
}

export function subscribe(fn: Subscriber): () => void {
  subscribers.add(fn);
  return () => subscribers.delete(fn);
}
