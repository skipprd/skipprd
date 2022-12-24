<?php

namespace Skipprd\Plugins\OffsetDrivers;

use Skipprd\Traits\Config;
use Skipprd\SkipprLogger;

class SkipprSaasOffsetDriver implements OffsetDriverInterface
{

    /**
     * Offsets currently committed to the backend. These will be at or behind the offsets during ingestion.
     * @var array
     */
    private $committedOffsets = [];

    public function get(): array
    {

        $this->committedOffsets = $this->client();

//        $this->committedOffsets = json_decode($body, true);

        return $this->committedOffsets;
    }

    public function sync(
        string $namespace,
        string $partition,
        string $offset
    ): void {

        $this->committedOffsets[$namespace][$partition] = $offset;

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

        $uri = Config::getenv('SKIPPR_API_ENDPOINT');

        $path = "/ingest-job/offsets/" . Config::$pipelineId;

        // Get Mapping
        try {
            $client = new \GuzzleHttp\Client([
                'base_uri' => $uri,
                'headers' => [
                    'Authorization' => "Bearer " . Config::getenv('SKIPPR_API_TOKEN')
                ]
            ]);

            switch ($method) {
                case 'PUT':
                    $uri = $uri . $path;

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
                    $body = json_decode($client->get($path)->getBody(), true);
                    $offsets = (!empty($body) ? $body : []);

                    return $offsets;
            }
        } catch (\Exception $e) {
            SkipprLogger::error($e->getMessage());
        }
    }
}
