<?php
// A shutdown function registered during a job runs at the end of that job, exactly once. The boot-registered one runs at cycle end.
$bootFired = 0;
$jobFired = 0;
register_shutdown_function(static function () use (&$bootFired): void {
    $bootFired++;
    \Rapira\log('job-fixture boot fired=' . $bootFired);
});

$req = 0;
$handler = static function () use (&$jobFired, &$req, &$bootFired): void {
    $req++;
    if ($req === 1) {
        register_shutdown_function(static function () use (&$jobFired): void {
            $jobFired++;
            \Rapira\log('job-fixture job fired=' . $jobFired);
        });
    }
    echo 'req=', $req, ' job_fired=', $jobFired, ' boot_fired=', $bootFired;
};
while (\Rapira\handle_request($handler)) {
}
