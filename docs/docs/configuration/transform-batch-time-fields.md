##### Config Name
TRANSFORM_BATCH_TIME_FIELDS

#### Description
This configuration determines the timestamp fields to be used for data ingestion in Skippr.

#### Default Value
By default, the value is empty which means no specific timestamp fields are chosen for data transformation.

#### Example Values
event_time,timestamp : In this scenario, Skippr will use both event_time and timestamp fields for data transformation.
timestamp : Here, only timestamp field will be used for data transformation.

#### Detailed Description
TRANSFORM_BATCH_TIME_FIELDS is a list of field names (separated by commas) that Skippr will use to extract timestamps from the data it ingests. It supports nested time fields using dot notation.

When processing each message, Skippr will loop through each field specified by TRANSFORM_BATCH_TIME_FIELDS in the order they are listed. It will use the value of the first field that it finds in the message as the timestamp. The value can either be a UNIX timestamp (in seconds or milliseconds) or a datetime string that can be parsed into a datetime.

If a timestamp field is not found or if the value of the field cannot be parsed into a valid timestamp, Skippr will not assign a timestamp to the message.

#### Considerations
If TRANSFORM_BATCH_TIME_UNIT is set and TRANSFORM_BATCH_TIME_FIELDS is empty, an error will be returned indicating that TRANSFORM_BATCH_TIME_FIELDS must be set.

Skippr recognizes timestamps in seconds and milliseconds. If a field's value is a number, Skippr will check if it is a valid UNIX timestamp in milliseconds and if so, convert it to seconds. If the value is not a valid timestamp in milliseconds, it will be treated as a timestamp in seconds.

Skippr tries to parse datetime strings into timestamps using a variety of common datetime formats. If the datetime string cannot be parsed, the field will be ignored.

The order of the fields in TRANSFORM_BATCH_TIME_FIELDS matters. Skippr will use the first field it finds in the message that contains a valid timestamp. If you want to prioritize certain fields over others, list them first.

Be careful when using nested fields. If a nested field does not exist in a message, it will be ignored. Make sure that the fields you specify in TRANSFORM_BATCH_TIME_FIELDS are present in your messages.
