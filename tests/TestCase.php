<?php

namespace Tests;

use Illuminate\Foundation\Testing\DatabaseMigrations;
use Illuminate\Foundation\Testing\RefreshDatabase;
use Illuminate\Foundation\Testing\TestCase as BaseTestCase;
use Illuminate\Support\Facades\DB;
use Illuminate\Support\Facades\File;

abstract class TestCase extends BaseTestCase
{
    use CreatesApplication;
//    use DatabaseMigrations;
    use RefreshDatabase;

    protected function setUp() {

        parent::setUp();

        $this->seed();

//        $path = database_path().'/tests/seed-db.sql';
//        $sql = file_get_contents($path);

//        DB::connection('sqlite_testing')->raw($sql);

    }
}
