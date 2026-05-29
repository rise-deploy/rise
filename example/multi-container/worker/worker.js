const Redis = require('ioredis');

const REDIS_ADDR = process.env.RISE_CONTAINER_HOST__REDIS || 'localhost:6379';
const WORK_MS = parseInt(process.env.WORK_MS || '2000', 10);
const QUEUE_KEY = 'jobs:pending';
const PID = process.pid;

// Two clients: BRPOP blocks the connection, so non-blocking writes need a
// separate one.
const blocker = new Redis(`redis://${REDIS_ADDR}`);
const writer = new Redis(`redis://${REDIS_ADDR}`);

const log = (obj) =>
  console.log(JSON.stringify({ ts: new Date().toISOString(), pid: PID, ...obj }));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function analyze(text) {
  const words = text.toLowerCase().match(/[a-z0-9']+/g) ?? [];
  const counts = new Map();
  for (const w of words) counts.set(w, (counts.get(w) ?? 0) + 1);
  const top_words = [...counts.entries()]
    .sort((a, b) => b[1] - a[1])
    .slice(0, 10)
    .map(([word, count]) => ({ word, count }));
  return {
    chars: text.length,
    words: words.length,
    unique_words: counts.size,
    top_words,
    reading_minutes: Math.max(1, Math.round(words.length / 200)),
  };
}

async function loop() {
  log({ msg: 'worker ready', redis: REDIS_ADDR, work_ms: WORK_MS });
  while (true) {
    try {
      const popped = await blocker.brpop(QUEUE_KEY, 5);
      if (!popped) continue;
      const id = popped[1];
      const key = `job:${id}`;
      const text = await writer.hget(key, 'text');
      if (text === null) {
        log({ msg: 'job missing (expired?)', id });
        continue;
      }
      await writer.hset(key, {
        status: 'processing',
        started_at: new Date().toISOString(),
        worker_pid: String(PID),
      });
      log({ msg: 'processing', id });
      await sleep(WORK_MS); // Simulate slow work so the queue is observable.
      const result = analyze(text);
      await writer.hset(key, {
        status: 'completed',
        completed_at: new Date().toISOString(),
        result: JSON.stringify(result),
      });
      log({ msg: 'completed', id, words: result.words });
    } catch (e) {
      log({ msg: 'error', error: String(e) });
      await sleep(1000);
    }
  }
}

loop().catch((e) => {
  log({ msg: 'fatal', error: String(e) });
  process.exit(1);
});
