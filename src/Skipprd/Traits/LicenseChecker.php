<?php

namespace Skipprd\Traits;

use Monolog\Registry;

trait LicenseChecker
{

    public $licenseIsValid = false;

    public $license = [];

    protected $licenseKey = 'none';

    protected $licenseApiKey = [
        'dev' => 'nFCBbKgf72pXNcKS9wKFA7n419Y9ql0J',
        'prod' => 'QYwmw6noVkPgCc5U3nWzW0e2PV2mP6HS',
    ];

    public function getLicense()
    {


        try {
            $this->licenseKey = Config::getenv('LICENSE_KEY', $this->licenseKey);

            SkipprLogger::info('Looking up license');

            $uri = Config::getenv('SKIPPR_API_ENDPOINT');

            if (!empty($uri)) {
                $authHeader = ['Authorization' => "Bearer " . Config::getenv('SKIPPR_API_TOKEN')];

            } elseif (empty($uri)) {
                $env = Config::getenv('APP_ENV', 'prod');

                $authHeader =  ['x-api-key' => $this->licenseApiKey[$env]];

                if ($env != 'prod') {
                    $uri = "https://license.$env.skippr.io";
                } else {
                    $uri = "https://license.skippr.io";
                }

            }


            $path = 'license-api/check/' . $this->licenseKey;

            $client = new \GuzzleHttp\Client([
                'base_uri' => $uri,
                'headers' => $authHeader

            ]);

            $body = $client->post($path)->getBody();

            $this->license = json_decode($body, true);

            // API only returns license that are currently valid
            if (!empty($this->license)
                && $this->license['license_key'] == $this->licenseKey
            ) {
                SkipprLogger::info('Found valid license');

                $this->licenseIsValid = true;
            } else {
                SkipprLogger::info('Did not find a valid license');

                $this->licenseIsValid = false;
            }
        } catch (\Exception $e) {
            SkipprLogger::error($e->getMessage());

            SkipprLogger::info('Did not find a valid license');

            $this->licenseIsValid = false;
        }
    }
}
