#!/bin/bash

#start the daemon manually
/usr/bin/newrelic-daemon -c /etc/newrelic/newrelic.cfg
wait

#Dummy request to connect the app to New Relic and give it a second to finish
php -i > /dev/null
sleep 1

#Start our actual script
php /usr/src/app/src/run.php
wait

#Give it some time to report data to New Relic before container shuts down
sleep 65