const express = require("express");
const multer = require("multer");
const path = require("path");
const {
  S3Client,
  ListObjectsV2Command,
  GetObjectCommand,
  PutObjectCommand,
  DeleteObjectCommand,
} = require("@aws-sdk/client-s3");

const app = express();
const PORT = process.env.PORT || 8080;
const BUCKET = process.env.S3_BUCKET_NAME;

if (!BUCKET) {
  console.error("S3_BUCKET_NAME environment variable is required");
  process.exit(1);
}

const s3 = new S3Client({});
const upload = multer({ storage: multer.memoryStorage(), limits: { fileSize: 50 * 1024 * 1024 } });

app.use(express.static(path.join(__dirname, "public")));

app.get("/api/files", async (req, res) => {
  const prefix = String(req.query.prefix || "");
  const delimiter = "/";
  try {
    const data = await s3.send(
      new ListObjectsV2Command({ Bucket: BUCKET, Prefix: prefix, Delimiter: delimiter })
    );
    const folders = (data.CommonPrefixes || []).map((p) => ({
      name: p.Prefix.slice(prefix.length),
      key: p.Prefix,
      type: "folder",
    }));
    const files = (data.Contents || [])
      .filter((obj) => obj.Key !== prefix)
      .map((obj) => ({
        name: obj.Key.slice(prefix.length),
        key: obj.Key,
        type: "file",
        size: obj.Size,
        lastModified: obj.LastModified,
      }));
    res.json({ prefix, items: [...folders, ...files] });
  } catch (err) {
    console.error("List error:", err);
    res.status(500).json({ error: err.message });
  }
});

app.get("/api/download", async (req, res) => {
  const key = req.query.key;
  if (!key) return res.status(400).json({ error: "key is required" });
  try {
    const data = await s3.send(new GetObjectCommand({ Bucket: BUCKET, Key: key }));
    res.set("Content-Type", data.ContentType || "application/octet-stream");
    res.set("Content-Disposition", `attachment; filename="${path.basename(key)}"`);
    data.Body.pipe(res);
  } catch (err) {
    console.error("Download error:", err);
    res.status(500).json({ error: err.message });
  }
});

app.post("/api/upload", upload.single("file"), async (req, res) => {
  if (!req.file) return res.status(400).json({ error: "No file provided" });
  const prefix = req.body.prefix || "";
  const key = prefix + req.file.originalname;
  try {
    await s3.send(
      new PutObjectCommand({ Bucket: BUCKET, Key: key, Body: req.file.buffer, ContentType: req.file.mimetype })
    );
    res.json({ key, size: req.file.size });
  } catch (err) {
    console.error("Upload error:", err);
    res.status(500).json({ error: err.message });
  }
});

app.delete("/api/files", async (req, res) => {
  const key = req.query.key;
  if (!key) return res.status(400).json({ error: "key is required" });
  try {
    await s3.send(new DeleteObjectCommand({ Bucket: BUCKET, Key: key }));
    res.json({ deleted: key });
  } catch (err) {
    console.error("Delete error:", err);
    res.status(500).json({ error: err.message });
  }
});

app.get("/health", (_req, res) => {
  res.json({ status: "healthy" });
});

app.listen(PORT, "0.0.0.0", () => {
  console.log(`S3 File Browser listening on port ${PORT} (bucket: ${BUCKET})`);
});
