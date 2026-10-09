/* The page's pure decision logic, ../dashboard-logic.js, under one name.
 * That file is a classic script, because `node --test tests/js` loads it
 * with require(). dashboard.html loads it before these modules, and it
 * defines window.HomeostatLogic. */
var logic = window.HomeostatLogic;
export default logic;

export var titleCase = logic.titleCase;
export var unitNameFromHealthKey = logic.unitNameFromHealthKey;
