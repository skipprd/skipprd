<?php

namespace Skipprd\Plugins\OffsetDrivers;

use Skipprd\Traits\Config;
use Skipprd\Traits\SkipprLogger;
use function Skipprd\value;


class SQLiteConnection {

    /**
     * PDO instance
     * @var type
     */
    private $pdo;

    /**
     * return in instance of the PDO object that connects to the SQLite database
     * @return \PDO
     */
    public function connect($dbFile) {
        if ($this->pdo == null) {
            $this->pdo = new \PDO("sqlite:" . $dbFile);

            $this->pdo->setAttribute(\PDO::ATTR_ERRMODE,
                \PDO::ERRMODE_EXCEPTION);
        }
        return $this->pdo;
    }
}

class SkipprSqliteOffsetDriver implements OffsetDriverInterface
{

    /**
     * @var \PDO
     */
    private $pdo;
    /**
     * Offsets currently committed to the backend. These will be at or behind the offsets during ingestion.
     * @var array
     */
    private $committedOffsets = [];

    protected $pipelineName = '';

    public function __construct()
    {
        $this->pipelineName = Config::getPipelineName();

        $pathToSqliteFile = Config::$dataDir . '/' . $this->pipelineName . '.db';

        $this->pdo = (new SQLiteConnection())->connect($pathToSqliteFile);
        $this->pdo->setAttribute(\PDO::ATTR_ERRMODE, \PDO::ERRMODE_EXCEPTION);

        if ($this->pdo != null) {
            SkipprLogger::info("Connected to the SQLite $this->pipelineName offset database successfully");

            try {
                $this->pdo->exec('CREATE TABLE IF NOT EXISTS offsets (namespace string, partition string, offset string, UNIQUE (namespace, partition))');

            }
            catch(\PDOException $e) {

                SkipprLogger::error($e->getMessage());
                exit(1);
            }

        } else {
            SkipprLogger::error("Could not connect to the SQLite $this->pipelineName database!");
            exit(1);
        }

    }

    protected function waitTransation(): void {
        $microseconds = 1;
        while ($this->pdo->inTransaction()) {
            usleep($microseconds);
            $microseconds++;
        }
    }

    public function getOffsets(string $namespace, array $partitions): array
    {

        $this->waitTransation();

        $this->pdo->beginTransaction();

        $offsets = [];

        $inPartitions  = str_repeat('?,', count($partitions) - 1) . '?';
        $sql = "SELECT offset FROM offsets WHERE namespace=? AND partition IN ($inPartitions)";
        $stm = $this->pdo->prepare($sql);
        $params = array_merge([$namespace], $partitions);
        $stm->execute($params);
        $offsets = $stm->fetchAll();

        $this->pdo->commit();

        if (empty($offsets[0])) {
            $offsets[0] = '';
        }

        return $offsets;

    }

    public function getOffset(string $namespace, string $partition = ''): array
    {

        $this->waitTransation();

        $this->pdo->beginTransaction();

        $offsets = [];

        $sql = "SELECT offset FROM offsets WHERE namespace = :namespace AND partition = :partition";

        $stmt = $this->pdo->prepare($sql);

        $stmt->bindParam(':namespace', $namespace);
        $stmt->bindParam(':partition', $partition);

        $offsets = $stmt->fetch();

        $this->pdo->commit();

        if (empty($offsets[0])) {
            $offsets[0] = '';
        }

        return $offsets;
    }

    public function offsetCommitAll(array $offsets): array
    {

        $sql = <<<EOF
INSERT OR REPLACE INTO offsets(namespace, partition, offset)
VALUES (:namespace, :partition, :offset)
EOF;

        $this->waitTransation();

        $this->pdo->beginTransaction();
        $statement = $this->pdo->prepare($sql);

        $namespace = key($offsets);
        foreach(array_chunk($offsets, 2) as $chunk) {

            foreach($chunk as $arr) {

                foreach($arr as $partition => $offset) {

                    $row = [
                        'namespace' => $namespace,
                        'partition' => $partition,
                        'offset' => $offset,
                    ];

                    $statement->execute($row);

                }
            }
        }

        $this->pdo->commit();

        return [];

    }

    public function resetSourceOffsets(): void
    {

        $this->waitTransation();

        $this->pdo->beginTransaction();
        $sql = 'TRUNCATE TABLE offsets';

        $stmt = $this->pdo->prepare($sql);

        $stmt->execute();

        $this->pdo->commit();

    }

    public function get(): array
    {

        $this->committedOffsets = $this->client();

        return $this->committedOffsets;
    }

    public function sync(string $namespace, string $partition, string $offset): void
    {
        $this->committedOffsets[$namespace][$partition] = $offset;

        $data = [
            'namespace' => $namespace,
            'partition' => $partition,
            'offset' => $offset,
        ];

        $this->client('PUT', $data);
    }

//    public function syncAll(array $offsets): void
//    {
//
//        $this->committedOffsets = $offsets;
//
//        $this->client('PUT', $this->committedOffsets);
//    }

    protected function client(string $method = 'GET', array $data = [])
    {

        try {
            switch ($method) {
                case 'PUT':
                    try {

                        $namespace = $data['namespace'];
                        $partition = $data['partition'];
                        $offset = $data['offset'];

                        $this->waitTransation();

                        $this->pdo->beginTransaction();

                        $sql = <<<EOF
INSERT OR REPLACE INTO offsets(namespace, partition, offset)
VALUES (:namespace, :partition, :offset)
EOF;
//                  $sql = "insert into offsets values (:namespace, :partition, :offset)";

                        $stmt = $this->pdo->prepare($sql);

                        $stmt->bindParam(':namespace', $namespace);
                        $stmt->bindParam(':partition', $partition);
                        $stmt->bindParam(':offset', $offset);

                        $stmt->execute();

                        $this->pdo->commit();

                        SkipprLogger::debug('Saved offsets to ' . Config::$pipelineName . 'SQLite DB');

                    } catch (\Exception $e) {
                        SkipprLogger::error($e->getMessage());
                    }

                    break;

                case 'GET':

                    $offsets = [];

                    try {

//                        $namespace = $data['namespace'];

                        $this->waitTransation();

                        $this->pdo->beginTransaction();

                        $sql = "SELECT * from offsets LIMIT 1";
                        $stmt = $this->pdo->query($sql);

//                        $stmt->bindParam(':namespace', $namespace);

                        $rows = $stmt->fetchAll();

                        $this->pdo->commit();

                        $committedOffsets = [];

                        foreach ($rows as $row) {

                            $committedOffsets[$row['namespace']][$row['partition']] = $row['offset'];

                            // Execute statement

                        }

                        return $committedOffsets;


                    } catch (\Exception $e) {
                        SkipprLogger::error($e->getMessage());
                    }

                    return $offsets;
            }
        } catch (\Exception $e) {
            SkipprLogger::error($e->getMessage());
        }
    }
}
