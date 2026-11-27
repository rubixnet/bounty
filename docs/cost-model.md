# Cost Model (Step 4 Gate 2)

All figures are monthly USD at **15,488 MAU / 109,759 hours a month** (Requirement 11). Targets: **≤ $300** at that load, and **≤ $500** under the load test's peak burst. Prices were read on **2026-11-27** from the pages named in each line. Where a price comes only from third-party pages or is an assumption, the line says so.

**Exchange rate (assumption):** 1 EUR = 1.16 USD. No live rate was fetched. Hetzner bills in EUR excluding VAT; re-check the rate and VAT treatment at purchase.

## What the load test measured

From `crates/traction-server/src/bin/load.rs` (the `load` binary; see `docs/deploy.md` for how it runs). Numbers below are the **reduced-scale smoke run** on a developer laptop (release build, 1,000 devices, ~200k seeded buckets, 150 concurrent devices, paced at 60x the real peak request rate). The full-scale report (15,488 devices, ~6.6M buckets) is produced by `.github/workflows/load.yml` and replaces these numbers when it runs; Gate 1 is ticked against that report.

| Measure | Smoke result | Gate |
|---|---|---|
| Ingest p95 (paced realtime + offline backlog chunks) | 7.5 ms | < 50 ms |
| Server RSS, peak | 21 MB | < 512 MB |
| Bytes per bucket, everything stored with it (buckets, rollups, Anchors, Timeline Marks, indexes) | **1,450 B** | input to the model |
| Synchronized burst of all 150 devices at once (informational) | 63 ms p95 | not gated |

## Postgres storage growth (from the measured bytes per bucket)

Monthly buckets: 109,759 h × 60 = 6.585M. At 1,450 B per bucket that is **9.5 GB a month**.

| Month | 1 | 3 | 6 | 9 | 12 |
|---|---|---|---|---|---|
| Cumulative GB | 9.5 | 28.6 | 57.2 | 85.9 | **114.5** |

The AX42's 2 x 512 GB NVMe in RAID 1 holds this with 4x headroom at month 12. Postgres runs on the server's local disks, so the storage line costs **$0** until a drive upgrade is needed (well past month 12). Use the nightly full-scale report's `bytes_per_bucket_all_in` to refresh this table.

## Itemized: 15,488 MAU (target ≤ $300)

| Line | Basis | $ / month | Source (read 2026-11-27) |
|---|---|---|---|
| Dedicated server | Hetzner AX42: €99.00/mo excl. VAT, Ryzen 7 PRO 8700GE, 64 GB DDR5 ECC, 2 x 512 GB NVMe, 1 Gbit port, unlimited traffic | 114.84 | https://www.hetzner.com/dedicated-rootserver/ax42/ and the AX line matrix https://www.hetzner.com/dedicated-rootserver/matrix-ax/ |
| Server setup fee | €49.00 one-time, spread over 12 months | 4.74 | same pages |
| Postgres storage growth | 114.5 GB at month 12 on local NVMe (table above) | 0.00 | load test; capacity from the Hetzner page |
| R2 storage, Audit Bundles | assume 10% of MAU submit one bundle a month, ~5 MB each: 7.7 GB/mo; 12-month retention = 93 GB; (93 − 10 GB free) × $0.015 | 1.20 | https://developers.cloudflare.com/r2/pricing/ (page modified 2026-10-30): Standard $0.015/GB-month, 10 GB free |
| R2 storage, `pg_dump` backups | compressed dump ≈ 25% of the 114.5 GB database = ~29 GB at month 12; keep 7 daily + 4 weekly + 3 monthly = 14 copies = ~400 GB × $0.015 | 6.00 | same R2 page |
| R2 operations | bundles ~3 ops each and backups ~300 multipart parts a day: ~15k Class A, ~10k Class B a month; free tier is 1M Class A and 10M Class B (Class A $4.50/M, Class B $0.36/M beyond that) | 0.00 | same R2 page |
| Jev at its spend limit | `[jev]` hard monthly spend limit set to **$40** (the line is priced at the limit, not at expected use). Expected use is ~$1: ~16k tokens of anonymized windows per submission × 1,549 submissions at ~$0.042 per million input tokens, output free | 40.00 | Third-party listings only (no official TypeSafe price page found): https://www.eesel.ai/blog/typesafe-jev-pricing, https://www.requesty.ai/models/typesafe/jev-1.14.0, https://vercel.com/ai-gateway/models/jev ($0.04–0.042 per 1M input tokens). **Verify with TypeSafe before relying on it.** |
| Bandwidth | Hetzner traffic is unlimited on the 1 Gbit port; R2 egress is free. Ingest is ~3.3M heartbeats × ~3 KB ≈ 10 GB/mo | 0.00 | the two pages above |
| TLS | Caddy with Let's Encrypt certificates | 0.00 | no fee |
| Domain | a `.com` at Cloudflare Registrar at cost, ~$11.17/yr ÷ 12 | 0.93 | Third-party price pages only (the Cloudflare page lists no figure): https://www.whtop.com/plans/cloudflare.com/135912 ($10.46/yr) and a reported Verisign increase to $10.97 effective 2026-11-01. Assumption: $11.17/yr. |
| Email | transactional mail from the free tier: 3,000 mails/mo, 100/day (one-off submitter notices; not per-heartbeat) | 0.00 | https://resend.com/pricing (page undated) |
| **Total** | | **167.71** | **≤ $300: passes with $132 to spare** |

(The line items sum to $167.71: 114.84 + 4.74 + 0 + 1.20 + 6.00 + 0 + 40.00 + 0 + 0 + 0.93 + 0.)

## Itemized: the load test's peak burst (ceiling ≤ $500)

The load test proves one AX42 absorbs peak concurrency (10x the ~153-device average, about 1,530 devices heartbeating at once) with large margin. This scenario nevertheless prices the **worst plausible month at peak**, assuming everything that scales does: a server **2.6x the price**, three times the submission volume, double the backup size, and a raised Jev limit.

| Line | Basis | $ / month |
|---|---|---|
| Dedicated server | Hetzner AX102: €259.00/mo, Ryzen 9 7950X3D, 128 GB DDR5 ECC (matrix page above) | 300.44 |
| Server setup fee | €129.00 one-time over 12 months | 12.47 |
| Postgres storage growth | local NVMe, as above | 0.00 |
| R2 storage, Audit Bundles | 30% of MAU submit: ~23 GB/mo, 285 GB at 12 months; (285 − 10) × $0.015 | 4.13 |
| R2 storage, backups | twice the baseline backup volume | 12.00 |
| R2 operations | three times the baseline, still inside the free tier | 0.00 |
| Jev at its spend limit | limit raised to **$100** | 100.00 |
| Bandwidth | unlimited on Hetzner, free R2 egress | 0.00 |
| Domain | as above | 0.93 |
| Email | Resend Pro, 50,000 mails/mo, $20 (https://resend.com/pricing) | 20.00 |
| **Total** | | **449.97** |

**≤ $500: passes with $50 to spare.** The sum is 300.44 + 12.47 + 4.13 + 12.00 + 100.00 + 0.93 + 20.00 = 449.97, (about $450).

## What can move these numbers

- **Jev** is priced at its configured hard limit because the spend limit is the cap the server enforces (falling back to `HeuristicEngine` when reached), not an estimate. The pricing source is third-party; confirm it before ticking Gate 2 for real.
- **Domain and exchange rate** change by cents and a few dollars; they do not threaten either ceiling.
- **Bytes per bucket** from the nightly full-scale run replaces the smoke figure. Even at 3x the measured size (4,350 B), month-12 storage is 343 GB, which still fits the AX42's 512 GB; the cost line stays $0 and the headroom shrinks.
- **Frames and Redaction (Step 5)** add blob storage that this model does not include; they are out of scope for Step 4.
