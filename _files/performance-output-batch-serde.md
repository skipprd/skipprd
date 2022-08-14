read buffer file into php array, then pass array to parquet serialise and iterate over
(method we've always used)

skipprd-output_1  | [2022-08-09T20:22:49+00:00] skipprd.DEBUG: Flushed buffer chunk buffer=output&namespace=&partition=
skipprd-output_1  | [2022-08-09T20:22:49+00:00] skipprd.DEBUG: Finalising output file /data/buffer/buffer=output&namespace=&partition=&temp_part
skipprd-output_1  | [2022-08-09T20:22:49+00:00] skipprd.DEBUG: Created lock on file /data/buffer/buffer=output&namespace=&partition=&temp_part
skipprd-output_1  | [2022-08-09T20:22:49+00:00] skipprd.DEBUG: Unpacking buffer file /data/buffer/buffer=output&namespace=&partition=&temp_part
skipprd_skipprd-source_1 exited with code 0
skipprd-output_1  | [2022-08-09T20:22:57+00:00] skipprd.DEBUG: Unpacked buffer file /data/buffer/buffer=output&namespace=&partition=&temp_part, serializing to parquetoutput format
skipprd-output_1  | [2022-08-09T20:30:23+00:00] skipprd.DEBUG: Output file /data/buffer/buffer=output&namespace=&partition=&temp_part finalised at 11.3KB and age of 0 seconds
skipprd-output_1  | [2022-08-09T20:30:23+00:00] skipprd.DEBUG: Destroying finished buffer file: /data/buffer/buffer=output&namespace=&partition=&temp_part

skipprd-output_1  | [2022-08-09T20:22:49+00:00] skipprd.DEBUG: Unpacking buffer file ...
skipprd-output_1  | [2022-08-09T20:30:23+00:00] skipprd.DEBUG: Output file ...

7.74


read buffer file and pass line striaght to parquet serialise

skipprd-output_1  | [2022-08-09T21:14:26+00:00] skipprd.DEBUG: Flushed buffer chunk buffer=output&namespace=&partition=
skipprd-output_1  | [2022-08-09T21:14:26+00:00] skipprd.DEBUG: Finalising output file /data/buffer/buffer=output&namespace=&partition=&temp_part
skipprd-output_1  | [2022-08-09T21:14:26+00:00] skipprd.DEBUG: Created lock on file /data/buffer/buffer=output&namespace=&partition=&temp_part
skipprd-output_1  | [2022-08-09T21:14:26+00:00] skipprd.DEBUG: Unpacking buffer file /data/buffer/buffer=output&namespace=&partition=&temp_part
skipprd_skipprd-source_1 exited with code 0
skipprd-output_1  | [2022-08-09T21:21:46+00:00] skipprd.DEBUG: Unpacked buffer file /data/buffer/buffer=output&namespace=&partition=&temp_part, serializing to parquet output format
skipprd-output_1  | [2022-08-09T21:21:46+00:00] skipprd.DEBUG: Output file /data/buffer/buffer=output&namespace=&partition=&temp_part finalised at 11.3KB and age of 0 seconds
skipprd-output_1  | [2022-08-09T21:21:46+00:00] skipprd.DEBUG: Destroying finished buffer file: /data/buffer/buffer=output&namespace=&partition=&temp_part

skipprd-output_1  | [2022-08-09T21:14:26+00:00] skipprd.DEBUG: Unpacking buffer file...
skipprd-output_1  | [2022-08-09T21:21:46+00:00] skipprd.DEBUG: Output file...

7.2 (so slightly faster but with much lower memory usage)