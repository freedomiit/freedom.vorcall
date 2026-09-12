using Xunit;

namespace Vorcall.Server.Tests.Infrastructure;

// Every test class joins this collection, and xunit.runner.json turns collection parallelism
// off: the suite shares one server, one database, one general channel and one owner account, so it
// runs serially.
[CollectionDefinition(Name)]
public sealed class ServerCollection : ICollectionFixture<ServerFixture>
{
    public const string Name = "vorcall-server";
}
