# Signal Intelligence & Incident Inference Platform

## 🔥 Motivation

Modern software and data systems generate massive volumes of telemetry: logs, metrics, user events, deployments, schema changes, etc. Despite abundant tooling, incident response remains **manual**, **slow**, and **highly reactive**. Teams are overwhelmed by noise and lack visibility into **what changed**, **why**, and **how it impacts upstream/downstream systems**.

This platform unifies **telemetry**, **asset graphs**, **code changes**, and **incident history** to infer incidents and **suggest root causes and blast radius**—without requiring perfect inputs. It gets better over time through **embeddings, ML, and pattern matching**.

## 🎯 Use Cases

- Detect silent data failures and software regressions
- Identify root cause and affected assets (services, tables, code)
- Correlate recent changes (GitHub, schema, deployment) with signals
- Shorten MTTD and MTTR through inference and graph traversal
- Learn from past incidents (Jira/Linear/etc.) to auto-detect future ones

## 📥 Input Events

- **Logs/Metrics/Traces/Alarms** — from any system (e.g. AWS CloudWatch, application logs, etc)
- **Deploy Events** — CI/CD, GitHub Actions, etc.
- **Schema Changes** — dbt, Athena/Hive, skippr Metadata, etc.
- **User Events** — product telemetry, event streams, CDC, Kinesis, EventBridge, etc
- **Configuration Changes** — feature flags, config files, etc.
- **Tickets** — Jira, Linear
- **Source Code Changes** — GitHub
- **Manual Tags/Metadata** — owner, team, environment
- **Infrastructure Changes** — cloud provider events, polling AWS APIs, CDK/Terraform in git, etc to map Assets

## 🏗️ Architecture

### Data Ingestion
- **S3 Input Plugin** — primary ingestion mechanism for EventBridge and Kinesis records
- **Batch Processing** — periodic S3 object processing for cost efficiency
- **Stream Processing** — real-time ingestion for critical signals
- **UDP** - application and server logs

### Query Engine
- **DataFusion SQL** — all analysis performed via DataFusion SQL queries
- **Custom Functions** — domain-specific SQL functions for signal analysis
- **Local Database** — embedded storage for metadata, stats, vectors, and computed results

### Processing Pipeline
```
S3 Objects → Skippr Ingest → Parquet DB → Analysis SQL → Incident Inference
     ↑            ↓           ↓           ↓              ↓
EventBridge/   Extract    Store     Custom SQL     Pattern
Kinesis       Metadata   Results   Functions      Detection
```

## 🧭 Workflow

@todo - ascii art workflow diagram

1. **Ingest Signals** (log/metric/etc.)
2. **Infer Metadata:** Timestamp, type, fields, stats, embedding
3. **Link to Assets** (based on payloads, time, context)
4. **Build Trace Graph** (causal edges)
5. **Match Events + Tickets**
6. **Run Pattern Detection + Embedding Similarity**
7. **Surface Incident Hypothesis with Blast Radius**

## 🗃️ Data Model

@todo - ascii art ERD diagram

### `RawSignal`
```yaml
id: string
source_type: enum(log|metric|event|trace)
source_id: string
env: string
payload: object
ingestion_time: datetime
customer_id: string
```

### `SignalMetadata`
```yaml
raw_signal_id: string # FK to RawSignal
inferred_type: string
timestamp: datetime
schema: object
embedding: vector<float>
```

### `Asset`
```yaml
id: string
kind: enum(service|table|repo|user|env)
owner: string
labels: map<string, string>
embedding: vector<float>
```

### `TraceEdge`
```yaml
id: string
from_id: string # FK to RawSignal, Asset, or Event
to_id: string # FK to RawSignal, Asset, or Event
relation: enum(causes|reads|writes|calls|depends_on)
timestamp: datetime
metadata: object
```

### `Event`
```yaml
id: string
type: enum(deploy|schema_change|config|release)
source: string
timestamp: datetime
metadata: object
```

### `Ticket`
```yaml
external_id: string
system: enum(jira|linear)
title: string
body: string
tags: list<string>
created_at: datetime
```

### `PatternRule`
```yaml
id: string
name: string
logic: string # executable logic or DSL
asset_scope: list<string>
severity: enum(info|warn|critical)
last_matched: datetime

```

## 🔧 Custom DataFusion SQL Functions

### Signal Analysis Functions
```sql
-- Extract embeddings from signal payloads
SELECT signal_embedding(payload) FROM raw_signals;

-- Compute time-series statistics
SELECT ts_stats(timestamp, value, '1h') FROM raw_signals;

-- Detect anomalies in metric streams
SELECT anomaly_score(timestamp, value, LAG(value, 10) OVER (ORDER BY timestamp)) 
FROM raw_signals WHERE source_type = 'metric';
```

### Asset Matching Functions
```sql
-- Match signals to assets using content similarity
SELECT asset_match(payload, embedding_threshold => 0.8) FROM raw_signals;

-- Graph traversal for blast radius
SELECT graph_traverse(asset_id, relation => 'depends_on', max_depth => 3) 
FROM assets WHERE id = ?;
```

### Pattern Detection Functions
```sql
-- Correlation analysis between events and signals
SELECT event_correlation(event_timestamp, signal_timestamp, time_window => INTERVAL '10 minutes')
FROM events e JOIN raw_signals s ON time_overlap(e.timestamp, s.timestamp);

-- Incident similarity matching
SELECT incident_similarity(current_signals, historical_patterns, similarity_threshold => 0.75);
```

## 🤖 Machine Learning Components

- **Signal Embeddings** — computed via custom SQL functions from statistical + LLM-derived context
- **Vector Search** — native DataFusion array operations for matching similar past incidents
- **Time Series Modeling** — custom SQL functions for field-wise anomaly detection
- **Co-occurrence Graphs** — SQL window functions and graph traversal for trace edge creation
- **Supervised Learning** — pattern rules stored as SQL queries, learned from Jira/Linear tickets

## 🧩 Integrations

- **S3 Input Plugin**: EventBridge and Kinesis record ingestion (current)
- **GitHub**: repo scanning, commit metadata via API polling
- **Jira / Linear**: ticket syncing with embedding generation
- **AWS CloudWatch**: telemetry forwarding to → Skippr Ingest -> DataFusion pipeline  
- **AWS APIs**: asset mapping, event polling stored in local DB
- **Skippr Metadata**: schema changes ingested via existing connectors
- **AWS CodePipeline / GitHub Actions**: deployment events via webhooks → Skippr Ingest -> DataFusion pipeline  

## 🚀 Implementation Phases

### Phase 1: Core DataFusion Pipeline
- S3 input plugin integration with DataFusion
- Basic signal ingestion and metadata extraction
- Local database schema and custom SQL functions (embedding, stats)

### Phase 2: Asset Mapping & Graph Building  
- Asset discovery from infrastructure APIs
- Trace edge creation using temporal correlation SQL
- Graph traversal functions for blast radius analysis

### Phase 3: Pattern Detection & ML
- Historical incident analysis from tickets
- Custom anomaly detection SQL functions
- Pattern rule engine with DataFusion queries

### Phase 4: Real-time Inference
- Streaming analysis for critical signals
- Automated incident hypothesis generation
- Integration with alerting systems

