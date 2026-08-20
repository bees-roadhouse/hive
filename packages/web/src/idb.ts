// idb.ts — the smallest usable promise skin over IndexedDB. Two stores:
// "reads" (GET response cache, keyed by request path) and "outbox" (queued
// writes, auto-increment keys so enqueue order IS key order). Deliberately not
// a library: the whole surface is get/put/del/entries/clear.
//
// Every helper degrades to a no-op / undefined when IndexedDB is unavailable
// (private windows, quota refusal) — the app then behaves exactly as it did
// before the offline layer existed.

const DB_NAME = "hive-offline";
const DB_VERSION = 1;
export const READS = "reads";
export const OUTBOX = "outbox";

let dbPromise: Promise<IDBDatabase | null> | null = null;

function openDb(): Promise<IDBDatabase | null> {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise((resolve) => {
    let req: IDBOpenDBRequest;
    try {
      req = indexedDB.open(DB_NAME, DB_VERSION);
    } catch {
      resolve(null);
      return;
    }
    req.onupgradeneeded = () => {
      const d = req.result;
      if (!d.objectStoreNames.contains(READS)) d.createObjectStore(READS);
      if (!d.objectStoreNames.contains(OUTBOX)) d.createObjectStore(OUTBOX, { autoIncrement: true });
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => resolve(null);
    req.onblocked = () => resolve(null);
  });
  return dbPromise;
}

export async function idbGet<T>(store: string, key: IDBValidKey): Promise<T | undefined> {
  const d = await openDb();
  if (!d) return undefined;
  return new Promise((resolve, reject) => {
    const r = d.transaction(store, "readonly").objectStore(store).get(key);
    r.onsuccess = () => resolve(r.result as T | undefined);
    r.onerror = () => reject(r.error);
  });
}

/** put without a key uses the store's key generator (outbox ordering). */
export async function idbPut(
  store: string,
  value: unknown,
  key?: IDBValidKey,
): Promise<IDBValidKey | undefined> {
  const d = await openDb();
  if (!d) return undefined;
  return new Promise((resolve, reject) => {
    const r =
      key === undefined
        ? d.transaction(store, "readwrite").objectStore(store).put(value)
        : d.transaction(store, "readwrite").objectStore(store).put(value, key);
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

export async function idbDel(store: string, key: IDBValidKey): Promise<void> {
  const d = await openDb();
  if (!d) return;
  return new Promise((resolve, reject) => {
    const r = d.transaction(store, "readwrite").objectStore(store).delete(key);
    r.onsuccess = () => resolve();
    r.onerror = () => reject(r.error);
  });
}

/** All entries with their keys, in key order (outbox replay order). */
export async function idbEntries<T>(
  store: string,
): Promise<{ key: IDBValidKey; value: T }[]> {
  const d = await openDb();
  if (!d) return [];
  return new Promise((resolve, reject) => {
    const out: { key: IDBValidKey; value: T }[] = [];
    const t = d.transaction(store, "readonly");
    const c = t.objectStore(store).openCursor();
    c.onsuccess = () => {
      const cur = c.result;
      if (cur) {
        out.push({ key: cur.key, value: cur.value as T });
        cur.continue();
      }
    };
    t.oncomplete = () => resolve(out);
    t.onerror = () => reject(t.error);
  });
}

export async function idbClear(store: string): Promise<void> {
  const d = await openDb();
  if (!d) return;
  return new Promise((resolve, reject) => {
    const r = d.transaction(store, "readwrite").objectStore(store).clear();
    r.onsuccess = () => resolve();
    r.onerror = () => reject(r.error);
  });
}
