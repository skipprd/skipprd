<?php

namespace Skipprd\Plugins\DataSources\OffsetDrivers;

use Monolog\Registry;
use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;

class SkipprInternalOffsetDriver implements OffsetDriverInterface
{

    /**
     * Offsets currently committed to the backend. These will be at or behind the offsets during ingestion.
     * @var array
     */
    private $committedOffsets = [];

    protected $pipelineName = '';

    public function __construct()
    {
        $this->pipelineName = Config::getPipelineName();
    }

    public function get() : array
    {

        $body = $this->client();

        $this->committedOffsets = json_decode($body, true);

        return   $this->committedOffsets;
    }

    public function sync(string $partition, string $offset) : void
    {

        $this->committedOffsets[$partition] = $offset;

        $this->client('PUT', $this->committedOffsets);
    }

//    public function syncAll(array $offsets) : void
//    {
//
//        $this->committedOffsets = $offsets;
//
//        $this->client('PUT', $this->committedOffsets);
//
//    }

    protected function client(string $method = 'GET', array $data = [])
    {

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

                    $response = $client->request(
                        'PUT',
                        $uri,
                        [
                            'json' => $data
                        ]
                    );

                    $body = $response->getBody();

                    break;

                case 'GET':
                    $body = $client->get($path)->getBody();
                    break;
            }
            
            return $body;
        } catch (\Exception $e) {
            SkipprLogger::error($e->getMessage());
        }
    }
}
