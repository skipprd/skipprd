##### TRANSFORM_NAMESPACE_FIELDS

#### Description
Defines the field(s) representing the event type name, allowing for the discovery of multiple schemas from the same source data.

#### Default Value
No default value.

#### Example Values
"eventType". This configuration will create a separate schema for each unique event type found in the data.
"eventCategory,eventType". If there are multiple comma-separated values, each unique combination of those fields will result in a distinct schema. In this case, each unique combination of eventCategory and eventType would generate a different schema.
"event_type,metadata.event_name". This configuration could be used to generate separate schemas for different event types with event names contained in a nested metadata field.
Detailed Description
####
The TRANSFORM_NAMESPACE_FIELDS configuration option instructs Skippr to treat the named fields as defining distinct event types. Skippr will create separate schemas for each unique value (or unique combination of values, if multiple fields are specified) in these fields. Each schema will be named according to these values.

In addition to controlling schema generation, this setting also impacts how output is batched and how tables are named. The values of the TRANSFORM_NAMESPACE_FIELDS fields are used to namespace the output batches and table names, thereby separating data into distinct outputs according to its event type.

#### Considerations
Select fields with a lower cardinality as every unique value in the specified field(s) will result in a new schema being generated.

It's also important to ensure that the selected fields are consistently present in the data and contain meaningful values, as missing or nonsensical values could result in schemas being inconsistently namespaced.

Lastly, the field name(s) must be correctly formatted as they appear in the source data, including any necessary nested value notations (dot-separated for JSON sources), and multiple fields should be comma-separated without spaces. For instance, for a nested field {"properties": {"eventType": "my_event_name"}} in a JSON source, the configuration should be "properties.eventType".
