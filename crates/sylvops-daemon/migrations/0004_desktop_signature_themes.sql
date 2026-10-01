UPDATE ui_state
SET value_json = replace(
    replace(value_json, '"theme":"light"', '"theme":"grove"'),
    '"theme": "light"',
    '"theme": "grove"'
)
WHERE client_scope = 'desktop'
  AND key = 'navigation.v1'
  AND (
      instr(value_json, '"theme":"light"') > 0
      OR instr(value_json, '"theme": "light"') > 0
  );

UPDATE ui_state
SET value_json = replace(
    replace(value_json, '"theme":"dark"', '"theme":"canopy"'),
    '"theme": "dark"',
    '"theme": "canopy"'
)
WHERE client_scope = 'desktop'
  AND key = 'navigation.v1'
  AND (
      instr(value_json, '"theme":"dark"') > 0
      OR instr(value_json, '"theme": "dark"') > 0
  );
