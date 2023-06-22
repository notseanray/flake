#### Full End to End CI

##### (intended) features:
* cypress for frontend testing
* full network tracing
* fully reproducable via docker
* fast & parallel where possible to reduce cost
* randomized tests
* json formatted logging to easily sort through

##### notes
for literally no reason macos uses port 5000 for "Control Center" which is used for Airplay and some random stuff, you can turn this off via System Prefences > AirDrop & Handoff > AirPlay Receiver (or just search for it)

to confirm this you can run `sudo lsof -i -P | grep LISTEN | grep :5000` to see what is using port 5000


##### quick reference links for future use
* https://www.mongodb.com/docs/database-tools/mongoexport/
* https://docs.docker.com/engine/reference/commandline/network_inspect/
