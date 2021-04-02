<?php


namespace Skipprd\Plugins\DataSources;


class Offsets
{

    protected string $offsets = '';

    public function __construct()
    {

    }

    public function setOffsets(string $offsets = '')
    {

        $this->offsets = $offsets;
    }

    public function getOffsets() {

        return $this->offsets;
    }

    public function parseOffsets()
    {

        $offsets = explode(' ', $this->offsets);

        if (empty($offsets[0])) {
            $offsets[0] = 0;
        }

        return $offsets;

    }

    public function validateOffset(string $args) : bool
    {

//        $offsets = $this->getOffsets();
//        return bccomp($args, $offsets, 5) == 1;


        $offsets = $this->parseOffsets();

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
                        $this->validateOffset($subArgs);
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
