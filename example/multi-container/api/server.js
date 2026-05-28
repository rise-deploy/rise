const express = require('express');
const Redis = require('ioredis');
const crypto = require('crypto');

const PORT = parseInt(process.env.PORT || '8080', 10);
// Auto-injected by the Rise controller for every routable sibling container.
// Falls back to localhost so the example can be run with `docker compose` too.
const REDIS_ADDR = process.env.RISE_CONTAINER_HOST__REDIS || 'localhost:6379';

const QUEUE_KEY = 'jobs:pending';
const INDEX_KEY = 'jobs:index';

const redis = new Redis(`redis://${REDIS_ADDR}`, { maxRetriesPerRequest: 3 });
const jobKey = (id) => `job:${id}`;

const app = express();
app.use(express.json({ limit: '256kb' }));

app.post('/api/jobs', async (req, res) => {
  const text = String(req.body?.text ?? '').slice(0, 50_000);
  if (!text.trim()) return res.status(400).json({ error: 'text is required' });

  const id = crypto.randomUUID();
  const fields = { id, status: 'pending', submitted_at: new Date().toISOString(), text };
  await redis
    .multi()
    .hset(jobKey(id), fields)
    .lpush(QUEUE_KEY, id)
    .zadd(INDEX_KEY, Date.now(), id)
    .exec();
  res.status(201).json(publicView(fields));
});

app.get('/api/jobs', async (_req, res) => {
  const ids = await redis.zrevrange(INDEX_KEY, 0, 24);
  if (ids.length === 0) return res.json({ jobs: [] });
  const pipe = redis.pipeline();
  for (const id of ids) pipe.hgetall(jobKey(id));
  const results = await pipe.exec();
  const jobs = results
    .map(([err, h]) => (err || !h || !h.id ? null : publicView(h)))
    .filter(Boolean);
  res.json({ jobs });
});

app.get('/api/jobs/:id', async (req, res) => {
  const h = await redis.hgetall(jobKey(req.params.id));
  if (!h?.id) return res.status(404).json({ error: 'not found' });
  res.json(publicView(h));
});

app.get('/api/health', async (_req, res) => {
  try {
    await redis.ping();
    res.json({ status: 'ok' });
  } catch (e) {
    res.status(503).json({ status: 'redis_down', error: String(e) });
  }
});

function publicView(h) {
  let result = null;
  if (h.result) {
    try { result = JSON.parse(h.result); } catch { /* leave null */ }
  }
  return {
    id: h.id,
    status: h.status,
    submitted_at: h.submitted_at,
    started_at: h.started_at ?? null,
    completed_at: h.completed_at ?? null,
    worker_pid: h.worker_pid ?? null,
    result,
    text_preview: (h.text ?? '').slice(0, 80),
  };
}

app.listen(PORT, '0.0.0.0', () => {
  console.log(`api listening on ${PORT}, redis at ${REDIS_ADDR}`);
});
