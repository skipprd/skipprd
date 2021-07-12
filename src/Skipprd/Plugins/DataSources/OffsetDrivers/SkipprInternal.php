<?php

namespace Skipprd\Plugins\DataSources\OffsetDrivers;

use Monolog\Registry;
use Skipprd\Traits\Config;

class SkipprApi implements OffsetDriverInterface
{

    protected $pipelineName = '';

    public function __construct()
    {
        $this->pipelineName = Config::getPipelineName();

    }

    public function get() : array {

        $body = $this->client();

        $offsets = json_decode($body, true);

        return $offsets;

    }

    public function sync(string $partition, string $offset) : void {

        $body = $this->client('PUT', [$partition => $offset]);

    }

    public function syncAll(array $offsets) : void {

        $body = $this->client('PUT', $offsets);

    }

    protected function client(string $method = 'GET', array $data = []) {

        $uri = Config::getenv('SCHEMA_REGISTRY');

        $path = "ingest-job/offsets/$this->pipelineName";

        // Get Mapping
        try {

            $url = "http://$uri/";

            $client = new \GuzzleHttp\Client([
                'base_uri' => $url,
                'headers' => [
                    'Authorization' => "Bearer " . Config::getenv('SCHEMA_API_TOKEN')
                ]
            ]);

            switch ($method) {
                case 'PUT':

                    $uri = $url . $path;

                    $response = $client->request('PUT',
                        $uri,
                        [
                            'json' => $data
                        ]);

                    $body = $response->getBody();

                    break;

                case 'GET':
                    $body = $client->get($path)->getBody();
                    break;
            }
            
            return $body;


        } catch (\Exception $e) {
            Registry::skipprd()
                ->error($e->getMessage());
        }
    }

}