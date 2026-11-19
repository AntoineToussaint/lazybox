// Keychain mutations cannot be cancelled once started. Serialize them and
// check ownership before starting a queued write and after it completes.
export class PairingLifecycle {
  private generation = 0;
  private writes = Promise.resolve();

  begin() { return ++this.generation; }
  get current() { return this.generation; }
  owns(ticket: number) { return ticket === this.generation; }

  write(ticket: number, operation: () => Promise<void>) {
    const result = this.writes.then(async () => {
      if (!this.owns(ticket)) throw new Error('Pairing changed.');
      await operation();
      if (!this.owns(ticket)) throw new Error('Pairing changed.');
    });
    this.writes = result.catch(() => {});
    return result;
  }
}
