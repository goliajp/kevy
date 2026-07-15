// These are integration tests: several classes spawn real kevy servers on
// their own ports. Running them in parallel contends for CPU during server
// startup and can trip the readiness deadline, so serialize the suite for
// deterministic runs.
[assembly: Xunit.CollectionBehavior(DisableTestParallelization = true)]
