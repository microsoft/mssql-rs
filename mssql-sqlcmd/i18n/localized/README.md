# Localized sqlcmd catalogs

OneLocBuild writes translated `sqlcmd.json` catalogs under language
folders in this directory, for example `de-DE/sqlcmd.json`.

Do not edit these files by hand. Update `../locales/en-US/sqlcmd.json`;
the localization pipeline creates or updates the localized-catalog pull request.
