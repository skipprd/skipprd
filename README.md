

## Development

##### PHP .4
Install PHP with required libs:

````
brew install automake autoconf curl pcre bison re2c mhash libtool icu4c gettext jpeg openssl libxml2 mcrypt gd gmp libevent zlib libzip bzip2 imagemagick pkg-config oniguruma
brew link --force icu4c
brew link --force openssl
brew link --force libxml2

export OPENSSL_PREFIX=$(brew --prefix openssl@3)
export OPENSSL_CFLAGS="-I${OPENSSL_PREFIX}/include"
export OPENSSL_LIBS="-L${OPENSSL_PREFIX}/lib -lcrypto -lssl"

phpbrew install -j 10 7.4.0 +gd +default +sqlite +mysql +dbs +zts +zlib +zlib=/usr/local/Cellar/zlib/1.2.11/ -- --with-gd=shared --with-openssl="${OPENSSL_PREFIX}" --enable-maintainer-zts --with-libedit 



LDFLAGS="-L/usr/local/opt/openssl@1.1/lib"
CPPFLAGS="-I/usr/local/opt/openssl@1.1/include"
OPENSSL_CFLAGS="-I/usr/local/opt/openssl@1.1/include"
OPENSSL_LIBS="-L/usr/local/opt/openssl@1.1/lib -lcrypto -lssl"
export PKG_CONFIG_PATH="/usr/local/opt/openssl@1.1/lib/pkgconfig"
export OPENSSL_PREFIX=$(brew --prefix openssl@1.1)
export CFLAGS=-DU_DEFINE_FALSE_AND_TRUE=1

phpbrew install -j 10 7.4.0 +gd +default +sqlite +mysql +dbs +zts +zlib +zlib=/usr/local/Cellar/zlib/1.2.11/ -- --with-gd=shared --with-openssl="${OPENSSL_PREFIX}" --enable-maintainer-zts --with-libedit



phpbrew ext install xdebug stable
phpbrew ext install soap stable
phpbrew ext install gmp stable
phpbrew ext install gd stable -- --with-zlib-dir=/usr/local/Cellar/zlib/1.2.11/
phpbrew ext install exif stable
phpbrew --debug ext install imagick stable -- --with-imagick=/usr/local/Cellar/imagemagick/7.0.9-27/
# intl specifications
export LDFLAGS="-L/usr/local/opt/icu4c/lib" 
export PKG_CONFIG_PATH=$(brew --prefix icu4c)/lib/pkgconfig
export CXX="g++ -DTRUE=1 -DFALSE=0"
export CC="gcc -DTRUE=1 -DFALSE=0"
LANG=C phpbrew ext install intl stable <--- not working, even after all the flag monkey business above

````

PHP 8.0
As per: https://github.com/phpbrew/phpbrew/issues/1249#issuecomment-1013031187
```
As above for PHP 7.4

phpbrew install -j 10 8.0 +default +dbs +zts +zlib -- --with-openssl="shared,${OPENSSL_PREFIX}" --enable-maintainer-zts --with-libedit

phpbrew switch php-8.0.0

phpbrew ext install openssl -- --with-openssl=$OPENSSL_PREFIX
LANG=C phpbrew ext install intl stable
```

PHP 8.1
```
As above for PHP 7.4

export LDFLAGS="-L/usr/local/opt/openldap/lib"
export CPPFLAGS="-I/usr/local/opt/openldap/include"


phpbrew install -j 10 8.1.7 +default +dbs +zts +zlib +bz2=/usr/local/Cellar/bzip2/1.0.8 -- --with-openssl="${OPENSSL_PREFIX}" --enable-maintainer-zts --with-libedit
```
 
## Local Build and Testing

1. Docker Login to pull base image

```
AWS_PROFILE=skippr docker login --username AWS --password $(AWS_PROFILE=skippr aws ecr get-login-password --region eu-west-2) 536671797322.dkr.ecr.eu-west-2.amazonaws.com
```

2. build with local tag

```
docker build --build-arg SSH_PRIVATE_KEY="${SSH_PRIVATE_KEY}" --platform=linux/amd64 -f ./Dockerfile -t skipprd:build .
docker build --platform=linux/amd64 -f ./Dockerfile -t skipprd:build .
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

