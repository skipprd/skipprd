<?php

namespace Skipprd\Traits;

use Monolog\Registry;

trait LicenseChecker
{

    public $licenseIsValid = false;

    public $license = [];

    protected $licenseKey = 'none';

//    protected $licenseApiKey = 'nFCBbKgf72pXNcKS9wKFA7n419Y9ql0J';

    public function getLicense()
    {


        try {

            $this->licenseKey = Config::getenv('LICENSE_KEY', $this->licenseKey);

            Registry::skipprd()->info('Looking up license');

            $uri = Config::getenv('SCHEMA_REGISTRY');

            $url = "http://$uri/";
            $path = 'check/'. $this->licenseKey;

            $client = new \GuzzleHttp\Client([
                'base_uri' => $url,
                'headers' => [
                    'Authorization' => "Bearer " . Config::getenv('SCHEMA_API_TOKEN')
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