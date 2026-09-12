namespace Vorcall.Server.Tests.Infrastructure;

// Rooms and accounts are persistent rows, so nothing in the suite may reuse a name: a second
// run against the same database would answer ROOM_EXISTS or "username is taken" instead of
// whatever the test is about. Prefixes are lower-case letters only, and none is a prefix of
// another, so the users listing sorts every name of a run the same way under any collation.
internal static class Names
{
    private static readonly string Run = Guid.NewGuid().ToString("N")[..6];

    private static int _counter;

    public static string Next(string prefix) => $"{prefix}-{Token()}";

    // Lower-case letters and digits only: usable inside a room name whose slug a test predicts.
    public static string Token() => $"{Run}{Interlocked.Increment(ref _counter)}";
}
