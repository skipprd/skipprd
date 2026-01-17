python3 -m venv .venv
source .venv/bin/activate
python -m pip install --no-cache-dir --index-url https://pypi.org/simple "dbt-core==1.10.0" "dbt-athena==1.10.0"
set AWS_PROFILE=circles-prod
set AWS_REGION=eu-west-1
dbt --version
echo “Run source .venv/bin/activate”
echo "or - run this script with 'source ./dbt-setup.sh'"
