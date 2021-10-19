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

            Registry::skipprd()->info('Looking up license');

            $uri = Config::getenv('SCHEMA_REGISTRY');

            if (!empty($uri)) {

                $authHeader = ['Authorization' => "Bearer " . Config::getenv('SCHEMA_API_TOKEN')];

                $url = "http://$uri/";
                
            } elseif (empty($uri)) {

                $env = Config::getenv('APP_ENV', 'prod');

                $authHeader =  ['x-api-key' => $this->licenseApiKey[$env]];

                if ($env != 'prod') {
                    $uri = "license.$env.skippr.io/license-api";

                } else {

                    $uri = "license.skippr.io/license-api";

                }

                $url = "https://$uri/";
            }


            $path = 'check/' . $this->licenseKey;

            $client = new \GuzzleHttp\Client([
                'base_uri' => $url,
                'headers' => $authHeader

            ]);

            $body = $client->post($path)->getBody();

            $this->license = json_decode($body, true);

            // API only returns license that are currently valid
            if (
                !empty($this->license)
                && $this->license['license_key'] == $this->licenseKey
            ) {

                Registry::skipprd()->info('Found valid license');

                $this->licenseIsValid = true;

            } else {

                Registry::skipprd()->info('Did not find a valid license');

                $this->licenseIsValid = false;
            }


        } catch (\Exception $e) {
            Registry::skipprd()->error($e->getMessage());

            Registry::skipprd()->info('Did not find a valid license');

            $this->licenseIsValid = false;
            
        }

    }

}