/* The page's pure decision logic, ../dashboard-logic.js, under one name.
 * That file stays a classic script, loaded by dashboard.html ahead of
 * these modules (it defines window.HomeostatLogic), because
 * `node --test tests/js` require()s it as it is. */
var logic = window.HomeostatLogic;
export default logic;

export var titleCase = logic.titleCase;
export var unitNameFromHealthKey = logic.unitNameFromHealthKey;
