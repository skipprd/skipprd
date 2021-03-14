###
## Builder
###

FROM 536671797322.dkr.ecr.eu-west-2.amazonaws.com/skippr-php:ubuntu-f9b2b780 as builder
#FROM skippr-php:ubuntu as builder

RUN df -h

##
# docker-php-extension-installer
##
ADD https://github.com/mlocati/docker-php-extension-installer/releases/latest/download/install-php-extensions /usr/local/bin/

RUN apt-get update -y && apt-get install -y netbase git openssh-client

RUN chmod +x /usr/local/bin/install-php-extensions \
    && sync
#    && install-php-extensions gd xdebug zip

ARG SSH_PRIVATE_KEY
RUN mkdir -p ~/.ssh
RUN echo "${SSH_PRIVATE_KEY}" > /root/.ssh/id_rsa
RUN chmod 700 ~/.ssh && chmod -R 600 ~/.ssh/*
RUN ssh-keyscan github.com >> ~/.ssh/known_hosts && chmod 644 ~/.ssh/known_hosts
RUN ssh-keyscan gitlab.com >> ~/.ssh/known_hosts && chmod 644 ~/.ssh/known_hosts

RUN install-php-extensions @composer

WORKDIR /usr/src/app
COPY ./src ./src
COPY ./composer.json ./
COPY ./composer.lock ./

#COPY --from=encoder /usr/src/encoded-app ./
#WORKDIR /usr/src/app

# parallel download and install of dependencies
#RUN composer global require hirak/prestissimo
RUN composer check-platform-reqs

RUN composer install --no-dev --no-interaction --no-ansi --prefer-dist --no-progress --optimize-autoloader

RUN rm -rf `find . -type f -name ".DS_Store" -print`
RUN rm composer.*

#############################################################

##
# encoder
##
#FROM php:7.4-cli as encoder

#WORKDIR /usr/src/app

COPY ./ioncube ./ioncube
#COPY --from=builder /usr/src/app/src ./source/

# NOTE: don't encode blade files, encrypt and replace
# See: https://blog.ioncube.com/2016/12/19/ioncube-encoding-laravel-project-controllers-models-templates/
# --encrypt '*.blade.php' \
#  --replace \
#
# obfuscation doesn't work
# --obfuscate all \
# --obfuscation-key "$(date +%s)" \
#
# --encrypt "*.php" \
# --binary \

#RUN mkdir -p /usr/src/encoded-app
#RUN mkdir -p /usr/src/encoded-modules

#--copy "@/*/" \
RUN ioncube/ioncube_encoder.sh --activate && \
    ioncube/ioncube_encoder.sh \
      -72 \
      --binary \
      --optimise "max" \
      --allow-reflection \
      --no-doc-comments \
      --disable-auto-prepend-append \
      # include attach prevention, only files ecoded with this secret can load encoded files
#      --property token=l59ctDBs7N0QQYBCBD3jeFFP0aEyX9CO \
#      --include-if-property "token='l59ctDBs7N0QQYBCBD3jeFFP0aEyX9CO'" \
      # obfuscate with a key. Left out:
      # - 'locals' as we use $$local_var variable variable assignments
      # - 'classes' to support class name based autoloading
      # - 'functions'
      --obfuscate linenos \
      --obfuscation-key "5L5GRWyUcVRljrWGJhSj4SJI3Uxb9Emx" \
      # copy by default, then only encode specific dirs
      --copy "@/*/" \
      --encode "src/Skipprd/Buffers/" \
      --encode "src/Skipprd/Commands/" \
      --encode "src/Skipprd/Converters/" \
      --encode "src/Skipprd/Serders/SerdersFactory.php" \
      --encode "src/Skipprd/Serders/SerderAvro.php" \
      --encode "src/Skipprd/Serders/SerderJson.php" \
      --encode "src/Skipprd/Serders/SerderCsv.php" \
      --encode "src/Skipprd/Traits/Config.php" \
      --encode "src/Skipprd/Traits/Ingest.php" \
      --encode "src/Skipprd/Traits/AnalyseSchema.php" \
      --encode "src/Skipprd/SkipprPack.php" \
#      --expire-in 180d \
      ./ \
      -o /usr/src/encoded-app/
#      --deactivate


RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Buffers/FileBuffer.php
RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Buffers/ChunkedBuffer.php

RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Commands/PipelineCommand.php

RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Converters/AvroHiveSchemaConverter.php
RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Converters/AvroParquetSchemaConverter.php
RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Converters/SkipprAvroSchemaConverter.php

RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Serders/SerdersFactory.php
RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Serders/SerderAvro.php
RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Serders/SerderJson.php
RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Serders/SerderCsv.php

RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Traits/Ingest.php
RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Traits/AnalyseSchema.php
RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Traits/Config.php

RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/SkipprPack.php

#RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Services/AvroSubPub/AvroProducer.php
#RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Services/AvroSubPub/CachedSchemaRegistryClient.php
#RUN grep -q --binary-files=text extension_loaded /usr/src/encoded-app/src/Skipprd/Services/AvroSubPub/MessageSerializer.php

RUN df -h

FROM 536671797322.dkr.ecr.eu-west-2.amazonaws.com/skippr-php:ubuntu-f9b2b780
#FROM skippr-php:ubuntu

WORKDIR /usr/src/app

COPY --from=builder /usr/src/encoded-app ./src

CMD ["php", "src/run.php"]
                  