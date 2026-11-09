// Shared by the tc scripts. Ryuk stays ON unless the caller sets
// TESTCONTAINERS_RYUK_DISABLED=true (marsh does not set it: DECISIONS 15).
// The Ryuk image is pinned by digest, so a services engine can have it cached.
process.env.RYUK_CONTAINER_IMAGE = process.env.RYUK_CONTAINER_IMAGE || "testcontainers/ryuk:0.5.1@sha256:533abc56c07b52a26c955d1e7ae428d810582ab01c156384ae79960eb5fa0775";

module.exports = {
  IMAGE: process.env.NFX_TC_IMAGE || "postgres:17@sha256:748a18a54d3842d43bc156f0840d78029b8d349e0feca98c12b9c4ce3404a6b3",
  RYUK_IMAGE: process.env.RYUK_CONTAINER_IMAGE,
};
