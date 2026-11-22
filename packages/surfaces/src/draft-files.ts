// Keep binary draft data in the window origin's database. The compact text
// draft references exact file identities, including for same-named files.
import type { ViewStateStore } from '@lazybox/platform';

type Entry = { id: string; name: string; type: string; lastModified: number; bytes: ArrayBuffer };
const pending = new Map<string, Promise<void>>();
const ids = new WeakMap<File, string>();
let database: Promise<IDBDatabase> | undefined;
function openDatabase() {
  if (!database) database = new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open('lazybox-composer-files', 1);
    request.onupgradeneeded = () => request.result.createObjectStore('drafts');
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  }).catch(error => { database = undefined; throw error; });
  return database;
}

export function draftFileId(file: File) {
  const id = ids.get(file) || crypto.randomUUID();
  ids.set(file, id);
  return id;
}

const key = (store: ViewStateStore, session: string) => store.key + ':draft:' + (session || 'new');

export function saveDraftFiles(store: ViewStateStore, session: string, files: File[]) {
  const draftKey = key(store, session);
  const previous = pending.get(draftKey);
  const save = async () => {
    // WebKit can refuse File objects in a private window. Persist their bytes.
    const entries = await Promise.all(files.map(async file => ({
      id: draftFileId(file), name: file.name, type: file.type,
      lastModified: file.lastModified, bytes: await file.arrayBuffer(),
    })));
    const db = await openDatabase();
    const tx = db.transaction('drafts', 'readwrite');
    const drafts = tx.objectStore('drafts');
    if (entries.length) drafts.put(entries, draftKey);
    else drafts.delete(draftKey);
    await new Promise<void>((resolve, reject) => {
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(tx.error);
    });
  };
  const done = previous ? previous.then(save, save) : save();
  pending.set(draftKey, done);
  return done.finally(() => { if (pending.get(draftKey) === done) pending.delete(draftKey); });
}

export async function loadDraftFiles(store: ViewStateStore, session: string) {
  await pending.get(key(store, session))?.catch(() => {});
  const db = await openDatabase();
  const request = db.transaction('drafts').objectStore('drafts').get(key(store, session));
  const entries = await new Promise<Entry[]>((resolve, reject) => {
    request.onsuccess = () => resolve(request.result || []);
    request.onerror = () => reject(request.error);
  });
  return new Map(entries.map(({ id, name, type, lastModified, bytes }) => {
    const file = new File([bytes], name, { type, lastModified });
    ids.set(file, id);
    return [id, file];
  }));
}
