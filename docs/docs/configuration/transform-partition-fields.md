##### Config Name `TRANSFORM_BATCH_PARTITION_FIELDS`

#### Description
Defines the field(s) for partitioning the ingested data.

#### Default Value
Not set ("")

#### Example Values

1. **"country,state"**: In this case, Skippr will use both `country` and `state` fields to create composite partitions. For instance, data with the same `country` and `state` will be placed in the same partition.

2. **"product.category,product.type"**: Similar to the above but supporting nested fields, creating a composite partition, but now based on `product.category` and `product.type`. Data sharing the same product category and type will belong to the same partition.

3. **"user.id"**: This will partition the ingested data based on the `user.id` field. If `user.id` for two data objects are same, they will go into the same partition. Typicaly you'd use a field with lower cardinality as this may create many partitions.

#### Detailed Description
The `TRANSFORM_BATCH_PARTITION_FIELDS` configuration allows you to specify one or more fields on which you want to partition the data Skippr ingests. This is particularly useful when dealing with large amounts of data, as it helps in managing and segregating the data more efficiently, which can be crucial for querying and processing.

The configuration accepts a string of comma-separated field names. Each field name corresponds to a value in the ingested data that will be used to partition the data.

When specifying multiple fields, Skippr will create composite partitions. That means it uses the combination of field values to create unique partitions. As a result, you can have a higher granularity of data partitioning.

#### Considerations
- When specifying multiple fields for composite partitions, the order of fields is significant. The partition "country,state" is different from "state,country".

- This configuration is case sensitive. "user.id" and "User.Id" will result in different partitions.

- If the specified field does not exist in the data, Skippr will create a partition with an empty value for that field.

- If the field value in the data is NULL, Skippr will consider it as a separate category and create a separate partition.

- Remember to use the correct field names and be consistent across your configurations to ensure that the partitioning works as expected.

- Finally, be aware of the impact of partitioning on your subsequent data processing. Depending on the granularity of your partitions, you might end up with a large number of small partitions or a small number of large partitions, each with its own performance considerations.
