All tests performed by sending messags to dev null

slow path
skipprd-source_1  | [2022-07-02T18:59:56+00:00] skipprd.INFO: Slow Path messages 16.6 K
skipprd-source_1  | [2022-07-02T19:00:07+00:00] skipprd.INFO: Slow Path messages 34.6 K
skipprd-source_1  | [2022-07-02T19:00:18+00:00] skipprd.INFO: Slow Path messages 52.6 K
skipprd-source_1  | [2022-07-02T19:00:29+00:00] skipprd.INFO: Slow Path messages 70.2 K
skipprd-source_1  | [2022-07-02T19:00:40+00:00] skipprd.INFO: Slow Path messages 88 K


fast path
avro schema check
skipprd-source_1  | [2022-07-02T19:19:19+00:00] skipprd.INFO: Fast Path messages 17 K
skipprd-source_1  | [2022-07-02T19:19:30+00:00] skipprd.INFO: Fast Path messages 35.1 K
skipprd-source_1  | [2022-07-02T19:19:41+00:00] skipprd.INFO: Fast Path messages 53.1 K
skipprd-source_1  | [2022-07-02T19:19:52+00:00] skipprd.INFO: Fast Path messages 71.4 K
skipprd-source_1  | [2022-07-02T19:20:03+00:00] skipprd.INFO: Fast Path messages 89.7 K
skipprd-source_1  | [2022-07-02T19:20:14+00:00] skipprd.INFO: Fast Path messages 108.1 K
skipprd-source_1  | [2022-07-02T19:20:25+00:00] skipprd.INFO: Fast Path messages 126.1 K

fast path
no avro check
skipprd-source_1  | [2022-07-02T19:26:07+00:00] skipprd.INFO: Fast Path messages 23.3 K
skipprd-source_1  | [2022-07-02T19:26:18+00:00] skipprd.INFO: Fast Path messages 46.9 K
skipprd-source_1  | [2022-07-02T19:26:29+00:00] skipprd.INFO: Fast Path messages 70.4 K
skipprd-source_1  | [2022-07-02T19:26:40+00:00] skipprd.INFO: Fast Path messages 94 K
skipprd-source_1  | [2022-07-02T19:26:51+00:00] skipprd.INFO: Fast Path messages 117.6 K
skipprd-source_1  | [2022-07-02T19:27:02+00:00] skipprd.INFO: Fast Path messages 141.2 K
skipprd-source_1  | [2022-07-02T19:27:13+00:00] skipprd.INFO: Fast Path messages 164.8 K

fast path -> fastSetValue
no avro check
skipprd-source_1  | [2022-07-02T20:56:04+00:00] skipprd.INFO: Fast Path messages 24.5 K
skipprd-source_1  | [2022-07-02T20:56:15+00:00] skipprd.INFO: Fast Path messages 49.5 K
skipprd-source_1  | [2022-07-02T20:56:26+00:00] skipprd.INFO: Fast Path messages 74.7 K
skipprd-source_1  | [2022-07-02T20:56:37+00:00] skipprd.INFO: Fast Path messages 100 K
skipprd-source_1  | [2022-07-02T20:56:48+00:00] skipprd.INFO: Fast Path messages 124.7 K
skipprd-source_1  | [2022-07-02T20:56:59+00:00] skipprd.INFO: Fast Path messages 149.7 K
skipprd-source_1  | [2022-07-02T20:57:10+00:00] skipprd.INFO: Fast Path messages 174.9 K
 --- various optmisations ---
skipprd-source_1  | [2022-07-02T21:21:11+00:00] skipprd.INFO: Fast Path messages 35 K
skipprd-source_1  | [2022-07-02T21:21:22+00:00] skipprd.INFO: Fast Path messages 72.3 K
skipprd-source_1  | [2022-07-02T21:21:33+00:00] skipprd.INFO: Fast Path messages 110.1 K
skipprd-source_1  | [2022-07-02T21:21:44+00:00] skipprd.INFO: Fast Path messages 147.6 K
skipprd-source_1  | [2022-07-02T21:21:55+00:00] skipprd.INFO: Fast Path messages 184.9 K
skipprd-source_1  | [2022-07-02T21:22:06+00:00] skipprd.INFO: Fast Path messages 222.7 K
skipprd-source_1  | [2022-07-02T21:22:17+00:00] skipprd.INFO: Fast Path messages 259.8 K



no ingest, just create array
skipprd-source_1  | [2022-07-02T19:29:32+00:00] skipprd.INFO: Fast Path messages 66.9 K
skipprd-source_1  | [2022-07-02T19:29:43+00:00] skipprd.INFO: Ingested messages 137.1 K
skipprd-source_1  | [2022-07-02T19:29:54+00:00] skipprd.INFO: Ingested messages 207 K
skipprd-source_1  | [2022-07-02T19:30:05+00:00] skipprd.INFO: Ingested messages 276.1 K
skipprd-source_1  | [2022-07-02T19:30:16+00:00] skipprd.INFO: Ingested messages 347.3 K
skipprd-source_1  | [2022-07-02T21:12:31+00:00] skipprd.INFO: Fast Path messages 415.5 K
skipprd-source_1  | [2022-07-02T21:12:42+00:00] skipprd.INFO: Fast Path messages 484.7 K






no ingest, just create array
no outputEmit()
skipprd-source_1  | [2022-07-02T21:14:45+00:00] skipprd.INFO: Fast Path messages 94.4 K
skipprd-source_1  | [2022-07-02T21:14:56+00:00] skipprd.INFO: Fast Path messages 189.4 K
skipprd-source_1  | [2022-07-02T21:15:07+00:00] skipprd.INFO: Fast Path messages 283.5 K
skipprd-source_1  | [2022-07-02T21:15:18+00:00] skipprd.INFO: Fast Path messages 377.3 K
skipprd-source_1  | [2022-07-02T21:15:29+00:00] skipprd.INFO: Fast Path messages 471.6 K
skipprd-source_1  | [2022-07-02T21:15:40+00:00] skipprd.INFO: Fast Path messages 564.9 K
skipprd-source_1  | [2022-07-02T21:15:51+00:00] skipprd.INFO: Fast Path messages 659.5 K


git checkout -b icu4c-69 c278d3dc42a6aac6ad7a46bd7d638c305364a888

brew switch icu4c 69.1

