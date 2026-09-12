namespace Vorcall.Server.Tests.Infrastructure;

// Accounts are persistent rows under a unique name, so nothing in the suite may reuse one: a
// second run against the same database would answer "username is taken" instead of whatever the
// test is about. Prefixes are lower-case letters only, and none is a prefix of another, so the
// members listing sorts every name of a run the same way under any collation. Channel names need
// not be unique, but a token in them still tells one run's channels from another's.
internal static class Names
{
    private static readonly string Run = Guid.NewGuid().ToString("N")[..6];

    private static int _counter;

    public static string Next(string prefix) => $"{prefix}-{Token()}";

    // Lower-case letters and digits only: usable inside a channel name as it stands, no trimming
    // and no control characters for the name grammar to refuse.
    public static string Token() => $"{Run}{Interlocked.Increment(ref _counter)}";
}
