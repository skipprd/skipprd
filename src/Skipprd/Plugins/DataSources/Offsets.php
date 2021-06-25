<?php


namespace Skipprd\Plugins\DataSources;


class Offsets
{

    protected array $offsets = [];

    public function __construct()
    {

    }

    public function setOffsets(string $partition, string $offsets = '')
    {

        $this->offsets[$partition] = $offsets;
    }

    public function getOffsets() {

        return $this->offsets;
    }

    public function parseOffsets(string $partition)
    {

        $offsets = [];
        
        if (!empty($this->offsets[$partition])) {

            $offsets = explode(' ', $this->offsets[$partition]);
        }


        if (empty($offsets[0])) {
            $offsets[0] = 0;
        }

        return $offsets;

    }

    public function validateOffset(string $partition, string $args) : bool
    {

//        $offsets = $this->getOffsets();
//        return bccomp($args, $offsets, 5) == 1;


        $offsets = $this->parseOffsets($partition);

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
                        $this->validateOffset($partition, $subArgs);
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
