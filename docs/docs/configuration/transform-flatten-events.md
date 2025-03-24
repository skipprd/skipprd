##### Skippr Configuration: TRANSFORM_FLATTEN_EVENTS

#### Config Name
TRANSFORM_FLATTEN_EVENTS

#### Description
Determines if Skippr should flatten nested ingested data.

#### Default Value
The default value is "no", which means Skippr will not flatten nested data by default.

#### Example Values
- "yes": Skippr will flatten nested ingested data, transforming it into a single layer structure.
- "no": Skippr will keep the nested structure of the ingested data intact.

#### Detailed Description
The TRANSFORM_FLATTEN_EVENTS configuration option allows Skippr to adjust the structure of the ingested data. When set to "yes", this configuration enables the flattening of nested data, transforming it into composite field names.

Flattening nested data can simplify your data structure and may help improve the efficiency of data querying or processing. Conversely, if nested data structure integrity is important for your use case, setting TRANSFORM_FLATTEN_EVENTS to "no" will maintain the original hierarchical structure of your data.

This option supports a variety of truth values, including "true", "t", "yes", "1", for enabling, and "false", "f", "no", "0" for disabling the feature.

##### Considerations
Data Structure: Consider the structure of your data and how you intend to use it. Flattening might simplify queries and data processing tasks. But, it could make it harder to spot hierarchical relationships inherent in your data, cause very long field names.

Performance: Depending on the complexity and depth of your data's nested structure, enabling TRANSFORM_FLATTEN_EVENTS might impact Skippr's processing performance. Assess the trade-off between data simplification and performance in your specific context.

Remember, setting TRANSFORM_FLATTEN_EVENTS is a global setting and will apply to all ingested data. If different treatments are needed for different data, consider using separate instances of Skippr with different configurations.
