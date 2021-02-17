<?php

namespace Unit\App\Plugins;

use App\IngestJob;
use App\Plugins\DataOutputs\DataOutputAWSAthena\AwsAthena;
use App\Schema;
use App\User;
use Illuminate\Foundation\Testing\Concerns\InteractsWithConsole;
use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Support\Facades\Auth;
use Mockery\Mock;
use Symfony\Component\Yaml\Yaml;
use Tests\TestCase;

class AwsAthenaMethodsTest extends TestCase
{

    public function setUp()
    {

        parent::setUp();

        $this->user = factory(User::class)->create([
            'name' => 'auth',
            'email' => 'auth@abc.com',
            'password' => randomPassword(12),
            'api_token' => 'fooooo',
            'tenant_id' => 'abc',
        ]);

    }

    public function testAvroTypeToParqueTypeDouble()
    {

        $schemaYml = <<<EOF
-
    name: skpr_event_ts
    default: null
    type: ['null', int]
-
    name: name
    default: null
    type: ['null', 'double']

EOF;

        $schema = Yaml::parse($schemaYml);

        $this->actingAs($this->user);
        $awsAthena = new AwsAthena(['aws_region' => 'eu-west-2']);

        $columns = $awsAthena->converter->convert($schema);

        $this->assertEquals('double', $columns[1]['Type']);
    }

    public function testAvroTypeToParqueTypeString()
    {

        $schemaYml = <<<EOF
-
    name: skpr_event_ts
    default: null
    type: ['null', int]
-
    name: name
    default: null
    type: ['null', 'string']

EOF;

        $schema = Yaml::parse($schemaYml);

        $this->actingAs($this->user);
        $awsAthena = new AwsAthena(['aws_region' => 'eu-west-2']);

        $columns = $awsAthena->converter->convert($schema);

        $this->assertEquals('string', $columns[1]['Type']);
    }

    public function testAvroTypeToParqueTypeLong()
    {

        $schemaYml = <<<EOF
-
    name: skpr_event_ts
    default: null
    type: ['null', int]
-
    name: phone
    default: null
    type: ['null', 'long']

EOF;

        $schema = Yaml::parse($schemaYml);

        $this->actingAs($this->user);
        $awsAthena = new AwsAthena(['aws_region' => 'eu-west-2']);

        $columns = $awsAthena->converter->convert($schema);

        $this->assertEquals('bigint', $columns[1]['Type']);
    }

    public function testAvroTypeToParqueTypeMapStrings()
    {

        $schemaYml = <<<EOF
- name: skpr_event_ts
  default: null
  type:
    - 'null'
    - int
- name: customer
  default: null
  type:
    - 'null'
    - type: record
      fields:
        - name: address
          default: null
          type:
            - 'null'
            - string
        - name: phone
          default: null
          type:
            - 'null'
            - string
        - name: email
          default: null
          type:
            - 'null'
            - string
        - name: company
          default: null
          type:
            - 'null'
            - string
        - name: name
          default: null
          type:
            - 'null'
            - type: map
              values: string
        - name: _id
          default: null
          type:
            - 'null'
            - string
      name: customer


EOF;

        $schema = Yaml::parse($schemaYml);

        $this->actingAs($this->user);
        $awsAthena = new AwsAthena(['aws_region' => 'eu-west-2']);

        $columns = $awsAthena->converter->convert($schema);

        $this->assertEquals('skpr_event_ts', $columns[0]['Name']);
        $this->assertEquals('int', $columns[0]['Type']);

        $this->assertEquals('customer', $columns[1]['Name']);

        $hiveString = 'struct<address:string,phone:string,email:string,company:string,name:map<string,string>,_id:string>';
        $this->assertEquals($hiveString, $columns[1]['Type']);


    }

    public function testAvroTypeToParqueTypeMapDouble()
    {

        $schemaYml = <<<EOF
- name: skpr_event_ts
  default: null
  type:
    - 'null'
    - int
- name: customer
  default: null
  type:
    - 'null'
    - type: record
      fields:
        - name: address
          default: null
          type:
            - 'null'
            - string
        - name: phone
          default: null
          type:
            - 'null'
            - string
        - name: email
          default: null
          type:
            - 'null'
            - string
        - name: company
          default: null
          type:
            - 'null'
            - string
        - name: name
          default: null
          type:
            - 'null'
            - type: map
              values: double
        - name: _id
          default: null
          type:
            - 'null'
            - string
      name: customer


EOF;

        $schema = Yaml::parse($schemaYml);

        $this->actingAs($this->user);
        $awsAthena = new AwsAthena(['aws_region' => 'eu-west-2']);

        $columns = $awsAthena->converter->convert($schema);

        $this->assertEquals('skpr_event_ts', $columns[0]['Name']);
        $this->assertEquals('int', $columns[0]['Type']);

        $this->assertEquals('customer', $columns[1]['Name']);

        $hiveString = 'struct<address:string,phone:string,email:string,company:string,name:map<string,double>,_id:string>';
        $this->assertEquals($hiveString, $columns[1]['Type']);


    }

    /**
     * Ensure once records nested fields don't end up in another record
     */
    public function testAvroTypeToParqueTypeRecordNotMergeFields()
    {

        $schemaYml = <<<EOF
- name: skpr_event_ts
  default: null
  type:
    - 'null'
    - int
- name: customer
  default: null
  type:
    - 'null'
    - type: record
      fields:
        - name: profile
          default: null
          type:
            - 'null'
            - type: record
              fields:
                - name: address
                  default: null
                  type:
                    - 'null'
                    - string
                - name: phone
                  default: null
                  type:
                    - 'null'
                    - string
                - name: email
                  default: null
                  type:
                    - 'null'
                    - string
                - name: company
                  default: null
                  type:
                    - 'null'
                    - string
                - name: name
                  default: null
                  type:
                    - 'null'
                    - type: map
                      values: double
        - name: location
          default: null
          type:
            - 'null'
            - type: record
              fields:
                - name: lat
                  default: null
                  type:
                    - 'null'
                    - double
                - name: lon
                  default: null
                  type:
                    - 'null'
                    - double
              
        - name: _id
          default: null
          type:
            - 'null'
            - string
      name: customer


EOF;

        $schema = Yaml::parse($schemaYml);

        $this->actingAs($this->user);
        $awsAthena = new AwsAthena(['aws_region' => 'eu-west-2']);

        $columns = $awsAthena->converter->convert($schema);

        $this->assertEquals('skpr_event_ts', $columns[0]['Name']);
        $this->assertEquals('int', $columns[0]['Type']);

        $this->assertEquals('customer', $columns[1]['Name']);

//        $hiveString = 'struct<address:string,phone:string,email:string,company:string,name:map<string,double>,_id:string>';
        $hiveString = 'struct<profile:struct<address:string,phone:string,email:string,company:string,name:map<string,double>>,location:struct<lat:double,lon:double>,_id:string>';
        $this->assertEquals($hiveString, $columns[1]['Type']);


    }

}