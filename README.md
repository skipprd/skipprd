

## Development

##### PHP .4
Install PHP with required libs:

````
export OPENSSL_PREFIX=$(brew --prefix openssl@1.1)
export OPENSSL_CFLAGS="-I${OPENSSL_PREFIX}/include"
export OPENSSL_LIBS="-L${OPENSSL_PREFIX}/lib -lcrypto -lssl"

export CFLAGS=-DU_DEFINE_FALSE_AND_TRUE=1

phpbrew install php-7.4.0 +default +dbs +zts +zlib +intl  -- --with-openssl="shared,${OPENSSL_PREFIX}" --enable-maintainer-zts --with-libedit 
 
 
# Install intl
export PKG_CONFIG_PATH=$(brew --prefix icu4c)/lib/pkgconfig
export CXX="g++ -DTRUE=1 -DFALSE=0"
export  CC="gcc -DTRUE=1 -DFALSE=0"
 
LANG=C phpbrew ext install intl -- --with-openssl=$OPENSSL_PREFIX -with-libdir=lib/x86_64-linux-gnu --enable-maintainer-zts --with-libedit 
phpbrew ext install intl -- --with-openssl=$OPENSSL_PREFIX -with-libdir=lib/x86_64-linux-gnu

````

PHP 8.0
As per: https://github.com/phpbrew/phpbrew/issues/1249#issuecomment-1013031187
```
export OPENSSL_PREFIX=$(export PHP_AUTOCONF=/usr/local/bin/autoconfbrew --prefix openssl@1.1)
export OPENSSL_CFLAGS="-I${OPENSSL_PREFIX}/include"
export OPENSSL_LIBS="-L${OPENSSL_PREFIX}/lib -lcrypto -lssl"

phpbrew --debug install -j 16 8.0 +default +dbs +zts +zlib -- --with-openssl="shared,${OPENSSL_PREFIX}" --enable-maintainer-zts --with-libedit

phpbrew switch php-8.0.0

phpbrew ext install openssl -- --with-openssl=$OPENSSL_PREFIX
```
 
## Local Build and Testing

1. Docker Login to pull base image

```
AWS_PROFILE=skippr docker login --username AWS --password $(AWS_PROFILE=skippr aws ecr get-login-password --region eu-west-2) 536671797322.dkr.ecr.eu-west-2.amazonaws.com
```
 
2. export ssh key to authenticate with gitlab for php composer
```
export SSH_PRIVATE_KEY=`cat ~/.ssh/skippr/id_rsa_deployer`
```

3. build with local tag

```
docker build --build-arg SSH_PRIVATE_KEY="${SSH_PRIVATE_KEY}" --platform=linux/amd64 -f ./Dockerfile -t skipprd:build .
```


## Integration Testing

The integration tests run against a container image. Expect a docker image named 'skippr/skipprd:build'.

Build as above in local build and then run integration tests 


## APM Profiling

*WARNING* tje `perf` build stage does NOT ioncube the source code, do not publish it!

Build skipprd to performance profile stage
```
docker build --target perf --build-arg SSH_PRIVATE_KEY="${SSH_PRIVATE_KEY}" --platform=linux/amd64 -f ./Dockerfile -t skipprd:perf .
```


Build with NewRelic Docker:run 

```
docker build --platform=linux/amd64 -f ./DockerfileNewRelic -t skipprd_with_newrelic --build-arg NEW_RELIC_AGENT_VERSION=10.0.0.312 --build-arg NEW_RELIC_LICENSE_KEY=504b373600890ad32d725c7dcd656e465bd63105 --build-arg NEW_RELIC_APPNAME=skipprd --build-arg IMAGE_NAME=skipprd:perf --progress=plain .
```

if ZTS (threading) then pin to lower agent version
```
docker build --platform=linux/amd64 -f ./DockerfileNewRelic -t skipprd_with_newrelic --build-arg NEW_RELIC_AGENT_VERSION=9.17.0.300 --build-arg NEW_RELIC_LICENSE_KEY=504b373600890ad32d725c7dcd656e465bd63105 --build-arg NEW_RELIC_APPNAME=skipprd --build-arg IMAGE_NAME=skipprd:perf --progress=plain .
```


export DATA_SOURCE_PLUGIN_NAME=file; export DATA_SOURCE_PATH='/data/input'; export DATA_SOURCE_FORMAT='json'; export DATA_SOURCE_MUTABLE_MODE='evolve'; export RUN_MODE=sync; export HOST='192.168.1.14'; export DATA_DIR='/data'; export TENANT_ID='skippr'; export PIPELINE_NAME='sample'; export PIPELINE_ID='1'; export LICENSE_KEY='94708298-498f-4c74-802c-ff359dd56cdf'; export LOG_LEVEL='INFO'; export APP_ENV=dev; docker run --platform=linux/amd64 skipprd_with_newrelic

export DATA_OUTPUT_PLUGIN_NAME: "file"; export DATA_OUTPUT_PATH: '/data/output'; export DATA_OUTPUT_TIME_BUCKET: day; export DATA_OUTPUT_TIME_FIELDS: 'detail_system_datetime_posix_utc_seconds'; export DATA_OUTPUT_PARTITION_BY_FIELDS: 'detail_type'; export DATA_OUTPUT_FORMAT: parquet; export RUN_MODE: sync; export DATA_OUTPUT_FLUSH_BYTES: '200000000'; export DATA_OUTPUT_FLUSH_SECONDS: '600'; export DATA_OUTPUT_FLUSH_RECORDS: '1000000'; export DATA_DIR: '/data'; export TENANT_ID: 'skippr'; export PIPELINE_NAME: 'sample'; export PIPELINE_ID: '1'; export LICENSE_KEY: '94708298-498f-4c74-802c-ff359dd56cdf'; export LOG_LEVEL: 'INFO'; export APP_ENV: dev; export MEM: 4096; php ./src/run.php


Start profiling container:


```
docker-compose -f ./docker-compose-newrelic.yml up --remove-orphans
```

- or - 

```
docker network create newrelic-php-test
docker run --platform=linux/amd64  -d --name newrelic-php-daemon --network newrelic-php-test newrelic/php-daemon
docker run --platform=linux/amd64 --network newrelic-php-test  skipprd_with_newrelic
```

