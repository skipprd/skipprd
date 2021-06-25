<?php
/**
 * Created by PhpStorm.
 * User: huders2000
 * Date: 01/09/2019
 * Time: 12:41
 */

namespace Unit\Skipprd\Traits\Ingest;

use Illuminate\Contracts\Container\Container;
//use Illuminate\Support\Facades\Log;
use Skipprd\Commands\PipelineCommand;
use Skipprd\Traits\AnalyseSchema;
use PHPUnit\Framework\ExpectationFailedException;
use Skipprd\Traits\Config;
use Superbalist\LaravelPubSub\PubSubConnectionFactory;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\DatabaseTransactions;
use Mockery;
use AvroSchema;

class ingestFieldEvolutionTest extends TestCase
{

    protected function setUp()
    {
        parent::setUp();

    }

    public $typeValues = [
        'integer' => [-1, -21474836, '-21474836', 0, 1, 12, 2147483646, '2147483646'],
        'long' => [41474836499, '41474836499', -41474836499, '-41474836499'],
        'double' => [-0.1, -9.99, 0.0, 0.1, 1.23, 8.97637849],
        'string' => ['abc', '123', '0.0'],
        'boolean' => [true, false, 1, 0],
    ];

    public function testCastMapping()
    {

        $container = Mockery::mock(PipelineCommand::class)->makePartial();
        $container->shouldReceive('AnalyseSchema');
        $container->shouldReceive('serder');

        foreach (AnalyseSchema::$dataTypeCasts as $castType => $casts) {

            foreach ($casts as $type) {

                if (empty($this->typeValues[$castType])) {
                    continue;
                }

                foreach ($this->typeValues[$type] as $typeValue) {

                    $fromType = gettype($typeValue);
                    if (!in_array($fromType, AnalyseSchema::$dataTypeCasts[$type])) {
                        continue;
                    }


//                foreach ($typeValues as $typeValue) {


                        $message = [
                            'foo' => $typeValue,
                        ];

                        $dataType = $container->resolveFieldType(Config::$discoveredFieldOccurrence, 'foo', $typeValue);

                        Config::$discoveredFieldOccurrence['foo_partition'] = [
                            'foo' => [
                                'count' => 2,
                                'type' => [
                                    $type => 1
                                ],
                                'fields' => [],
                                'last_value' => $typeValue,
//                                'determined_type' => $type,
                                'determined_type' => 'none-type',
                                'evolution' => [
                                    $dataType => [
                                        'type' => 'cast',
                                        'new_value' => $castType
                                    ]
                                ]
                            ]
                        ];

                        foreach ($message as $field => $value) {
                            $container->ingestField($field, $value,
                                Config::$discoveredFieldOccurrence['foo_partition'],
                                $message);
                        }

                        $from = var_export($typeValue, true);
                        $fromType = gettype($typeValue);
                        $to = var_export($message['foo'], true);
                        $toType = gettype($message['foo']);


//                    $this->assertEquals($castType, $container->getLogicalType('foo', $message['foo']));

                        if ($fromType != $type && $fromType != $castType) {

//                            $this->assertNotEquals($typeValue, $message['foo']);

//                            $castType = ($castType == 'long') ? 'integer' : $castType;

//                            $newDataType = $container->resolveFieldType( Config::$discoveredFieldOccurrence, 'foo', $message['foo']);
                            $newDataType = gettype($message['foo']);

                            try {
                                $this->assertEquals($castType, $newDataType);

                            } catch (ExpectationFailedException $e) {

                                    $this->addWarning("Expecting ($type) $typeValue, cast: ($fromType) $from to $castType: ($toType) $to");

                                    throw new ExpectationFailedException($e);

                            }

                        }

//                    }
                }

            }

        }

    }

}