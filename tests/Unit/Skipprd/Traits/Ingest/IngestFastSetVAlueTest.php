<?php

namespace Skipprd\Traits;


use PHPUnit\Framework\TestCase;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Converters\SkipprAvroSchemaConverter;
use Skipprd\InternalFields;

class IngestFastSetVAlueTest extends TestCase
{

    public function testFastSetValue()
    {
        $container = \Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

        $message = [
            'customer' => [
                'address' => '509 Kings Hwy, Comptche, Missouri, 4848',
                'phone' => '+1 (999) 407-2274',
                'email' => 'blankenship.patrick@orbin.ca',
                'company' => 'ORBIN',
                'name' => [
                    'last' => 'Patrick',
                    'first' => 'Blankenship',
                ],
                '_id' => '5730864df388f1d653e37e6f',
                'metadata' => [
                    'event_time' => 0001,
                ]
            ],
        ];

        $message['skpr_namespace'] = 'foo_namespace';
        $message['skpr_partition'] = 'gdh';
        $message['source_namespace'] = 'foo_namespace';
        $message['source_partition'] = 'dh';

        /**
         * analyse
         */
        Config::$discoveredFieldOccurrence['foo_namespace']['fields'] = [];
        $container->analysePayload($message, Config::$discoveredFieldOccurrence['foo_namespace']['fields']);
        $container->determineFieldTypes(Config::$discoveredFieldOccurrence['foo_namespace']['fields']);
        $metadata = Config::$discoveredFieldOccurrence['foo_namespace']['fields'];

        /**
         * ingest
         */
        Config::$tenantId ='foo';
        Config::$pipelineName = 'bar';
        $converter = new SkipprAvroSchemaConverter();
        Config::$schema['foo_namespace'] = $converter->convert($metadata);

        Config::$schema['foo_namespace'] = Config::schemaMerge(
            Config::$specialFieldsMapping,
            Config::$schema['foo_namespace']
        );

        Config::$avroSchemas['foo_namespace'] = Config::buildAvroSchema(Config::$schema['foo_namespace']);


        $ingestedMessage = [];

        foreach ($message as $field => $value) {

            $ingestedMessage[$field] = $container->fastSetValue(
                $metadata[$field]['determined_type'],
                $field,
                $value,
                $metadata
            );

        }

        var_dump(Config::$schema['foo_namespace'][5]);
//        $this->assertEquals($ingestedMessage, $message);


//        try {
            $valid = \AvroSchema::is_valid_datum(Config::$avroSchemas['foo_namespace'], $ingestedMessage);
//        } catch (\Exception $e) {
//            $this->throwException($e);
//        }
        $this->assertTrue($valid);
//        $this->assertEquals($ingestedMessage['customer']['metadata']['event_time'], 0001);
//        $this->assertNotEquals($message['skpr_event_ts'], 0001);
//        $this->assertEquals($message['skpr_event_ts'], 0);
    }
}
