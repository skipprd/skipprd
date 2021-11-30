<?php


namespace Skipprd\Plugins\DataSources;

class Offsets
{

    protected array $offsets = [];

    public function setOffsets(string $offsets, string $namespace, string $partition = '0')
    {

        $this->offsets[$namespace][$partition] = $offsets;
    }

    public function getOffsets(string $namespace, string $partition = '0')
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

    public function getCurrentOffsets(string $namespace, string $partition = '0')
    {
        return $this->offsets[$namespace][$partition];
    }

    public function validateOffset(string $args, string $namespace, string $partition = '0') : bool
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
