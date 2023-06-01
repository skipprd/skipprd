# Soda Data Testing

### Getting Started

The soda interactive demo is unmissable and speaks a thousand words.

https://docs.soda.io/soda/core-interactive-demo.html

##### Run this projects checks

1. Install Soda CLI

```
https://docs.soda.io/soda/core-interactive-demo.html
```

2. Run the check defined in this project, against the CloudCycle datalake.

```
soda scan -d datalake_dev -c configuration.yml checks_location.yml
```

### Limitations

Generally, I've found soda is good for validating schema and correctness. 

It's been easier and cleaner to define than Great Expectations with seemingly the same features. 

It's been easy to assert:
- fields must be of type, not null, exist
- fields values must not be duplicated

Consistency and completness has been harder when checking time series.
Soda's filtering features are not fully supported by every type of check and so scoping by time window often won't assert how you might expect.

For instance:

soda is designed for this sort of check of a complete set:

```
- values in (invoice.order_id) must exist in orders (id)
```

But struggles with:

```
- values in (device_data.vehicle_reg) must exist in dockets (vehicle_reg)
    filter: time_utc between DATE_ADD('hour', -1, NOW()) AND NOW()
```

**freshness(system_datetime_posix_utc_seconds) < 1h**

```
Could not evaluate freshness: max(time) is not a datetime: str
```

Solution would be to store dates as Parquet `timestamp` and Hive `timestamp`:

https://docs.aws.amazon.com/athena/latest/ug/data-types.html
https://github.com/apache/parquet-format/blob/master/LogicalTypes.md