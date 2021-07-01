<?php

namespace Skipprd\Traits;

use Monolog\Registry;

trait LicenseChecker
{

    public $licenseIsValid = false;

    public $license = [];

    protected $licenseKey = 'none';

    protected $licenseApiKey = 'nFCBbKgf72pXNcKS9wKFA7n419Y9ql0J';

    public function getLicense()
    {


        try {

            $this->licenseKey = Config::getenv('LICENSE_KEY', $this->licenseKey);

            Registry::skipprd()->info('Looking up license');

            $env = Config::getenv('APP_ENV', 'prod');

            if ($env != 'prod') {
                $uri = "license.$env.skippr.io/license-api";
                
            } else {

                $uri = "license.skippr.io/license-api";
                
            }


            $url = "https://$uri/";
            $path = 'check/'. $this->licenseKey;

            $client = new \GuzzleHttp\Client([
                'base_uri' => $url,
                'headers' => [
                    'x-api-key' => $this->licenseApiKey
                ]
            ]);

            $body = $client->get($path)->getBody();

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