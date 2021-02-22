<?php

namespace Skipprd\Plugins\DataOutputs;

use Skipprd\Buffers\FileBuffer;

Interface DataOutputPluginInterface
{

    public function __construct(array $config, FileBuffer $buffer);

    public function doValidateConnection(array $config);

    public function doValidateConfig(array $config);

    public function doSave(array $config);

    public function createOrUpdateSchema(array $schema);

    public function sync(string $serde = '', \AvroSchema $schema = null);
    
    public function execQuery(array $config);

    public function deleteSchema();

    public function deletePlugin();

    public function shutdown();

}