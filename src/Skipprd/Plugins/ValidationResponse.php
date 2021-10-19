<?php


namespace Skipprd\Plugins;


class ValidationResponse
{

    public bool $result;
    public string $title;
    public array $data;
    public string $error;


    public function __construct(string $title)
    {
        $this->title = $title;
        $this->result = true;
    }

    public function __set($name, $value)
    {
        if ($name == 'error') {
            $this->result = false;
        }
    }
}
