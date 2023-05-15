<?php

namespace Unit\Skipprd\Plugins;

use legacy\src\Skipprd\Plugins\OffsetDrivers\OffsetDriverFactory;
use legacy\src\Skipprd\Plugins\OffsetDrivers\OffsetDriverInterface;
use legacy\src\Skipprd\Plugins\Offsets;
use PHPUnit\Framework\TestCase;

class OffsetsTest extends TestCase
{

//    public function testGetOffsetsNew()
//    {
//
//        $namespace = 'table_a';
//        $partition = 'shard_1';
//
//        $offsets = new Offsets();
//
//        $commits = $offsets->getOffsets($namespace, $partition);
//
//        self::assertEquals([0], $commits);
//
//    }

    /**
     * @var \legacy\src\Skipprd\Plugins\OffsetDrivers\OffsetDriverInterface
     */
    protected OffsetDriverInterface $driver;

    public function setup(): void {
        parent::setup();

        $this->driver = OffsetDriverFactory::factory('skippr_file');
    }

    public function testParseOffsets()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets('123 123', $namespace, $partition);

        $commits = $offsets->getOffset($namespace, $partition);

        self::assertEquals([123, 123], $commits);

    }

    public function testParseSinglePartitionOffsets()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets('123', $namespace, $partition);

        $commits = $offsets->getOffset($namespace, $partition);

        self::assertEquals([123], $commits);

    }

    /**
     * Validate offsets
     */

    public function testValidateOffsetFloatDecimalLess()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 12', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 123 4', $namespace, $partition);

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetFloatDecimalGreater()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 4', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 123 12', $namespace, $partition);

        self::assertEquals(true, $valid);

    }

    public function testValidateOffsetBothLess()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 123', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 122 122', $namespace, $partition);

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetMinorLess()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 123', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 123 122', $namespace, $partition);

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetMajorLess()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 123', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 122 123', $namespace, $partition);

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetBothEqual()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 123', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 123 123', $namespace, $partition);

        self::assertEquals(false, $valid);

    }

    public function testValidateOffsetBothGreater()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 123', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 124 124', $namespace, $partition);

        self::assertEquals(true, $valid);

    }

    public function testValidateOffsetMinorGreater()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 123', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 123 124', $namespace, $partition);

        self::assertEquals(true, $valid);

    }

    public function testValidateOffsetMajorGreater()
    {

        $namespace = 'table_a';
        $partition = 'shard_1';

        $offsets = new Offsets($this->driver);
        $offsets->setOffsets(' 123 123', $namespace, $partition);
        $offsets->offsetCommitLatest($namespace);

        $valid = $offsets->validateOffset(' 124 123', $namespace, $partition);

        self::assertEquals(true, $valid);

    }

}
