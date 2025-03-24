##### TRANSFORM_BATCH_TIME_UNIT Configuration

#### Config Name
TRANSFORM_BATCH_TIME_UNIT

#### Description
Defines the unit of time to be used for the transformation of batch data.

#### Default Value
The default value is not set. If it's not defined, it is considered as empty.

#### Example Values
- "year": The transformation batch will be grouped based on the year.
- "month": The transformation batch will be grouped based on the month.
- "day": The transformation batch will be grouped based on the day.
- "hour": The transformation batch will be grouped based on the hour.
- "minute": The transformation batch will be grouped based on the minute.

#### Detailed Description
TRANSFORM_BATCH_TIME_UNIT is a configuration setting in Skippr that defines the unit of time to be used when processing and transforming the batch of data. This setting controls how the data is grouped and processed by timestamp. If this configuration is set, another environment variable, `TRANSFORM_BATCH_TIME_FIELDS`, must also be set. If not, it will result in an error. The value for `TRANSFORM_BATCH_TIME_UNIT` can be one of the following: "year", "month", "day", "hour", "minute". This selection will determine the granularity of the timestamp transformation.

#### Considerations
It's important to set the TRANSFORM_BATCH_TIME_UNIT value considering your data and the granularity level that best suits your transformation needs. Remember that a finer granularity, such as "minute", will result in more, smaller groups of data, whereas a coarser granularity, like "year", will result in fewer, larger groups. The finer the granularity, the more resources Skippr might require to process the data.

Additionally, the configuration relies on another setting `TRANSFORM_BATCH_TIME_FIELDS`. If TRANSFORM_BATCH_TIME_UNIT is defined and `TRANSFORM_BATCH_TIME_FIELDS` is not, Skippr will output an error. To avoid this, ensure both are properly defined when using the time-based batch transformation feature.
