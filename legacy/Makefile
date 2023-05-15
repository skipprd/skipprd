#@PYTHONPATH="$(PYTHONPATH):lambda-layers"

PLUGIN_NAME=File

all: install lint check test


install:
	@composer install

lint:
	@composer phpcbf

check:
	@composer phpstan; \
	composer phpcs

test:
	@composer phpunit
