<?php


namespace Skipprd\Plugins;

use Skipprd\InternalFields;
use Skipprd\Traits\SkipprLogger;

class Offsets
{

    protected array $offsets = [];

    public function getAll(): array
    {

        return $this->offsets;
    }

    public function setOffsets(string $offsets, string $namespace, string $partition = '')
    {

        try {
            $this->offsets[$namespace][$partition] = $offsets;
        } catch (\Exception $e) {
            SkipprLogger::error($e->getMessage());
            SkipprLogger::error("Namespace: $namespace, Partition: $partition, Offsets: $offsets");

        }
    }

    public function getOffsets(string $namespace, string $partition = '')
    {

        $offsets = [];
        
        if (!empty($this->offsets[$namespace][$partition])) {
            $offsets = explode(' ', $this->offsets[$namespace][$partition]);
        }


        if (empty($offsets[0])) {
            $offsets[0] = 0;
        }

        return $offsets;
    }

    public function getCurrentOffsets(string $namespace, string $partition = '')
    {
        return $this->offsets[$namespace][$partition];
    }

    public function validateOffset(string $args, string $namespace, string $partition = '') : bool
    {

        //        $offsets = $this->getOffsets();
        //        return bccomp($args, $offsets, 5) == 1;
        
        $offsets = $this->getOffsets($namespace, $partition);

        $args = explode(' ', $args);

        $i = 0;
        $total = count($args);

        while ($i < $total) {
            if ($args[$i] >= $offsets[$i]) {
                if ($args[$i] == $offsets[$i]) {
                    $next = $i + 1;

                    if ($total > $next) {
                        $subArgs = array_slice($args, $next);
                        $subArgs = implode(' ', $subArgs);
                        $this->validateOffset($subArgs, $namespace, $partition);
                    }
                } else {
                    return true;
                }
            } else {
                return false;
            }

            $i++;
        }

        return false;
    }
}
