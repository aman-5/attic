global using Demo.Helpers;
using static Demo.Helpers.MathHelpers;
using Alias = Demo.Models.Widget;

namespace Demo.Services
{
    // NotReal AlsoFake fake()
    public interface IWorker : IDisposable
    {
        void Run();
        int Value { get; }
    }

    public class BaseWorker
    {
        public void Common() { }
    }

    public readonly record struct AuditRecord(int Id);

    public struct Job : IWorker
    {
        public int Value { get; }

        public Job(int value)
        {
            Value = value;
        }

        public void Run()
        {
            var widget = new Alias();
            var again = new Job(1);
            Common();
            Max(Value, 1);
        }

        public void Common() { }
    }

    public class Worker : BaseWorker, IWorker
    {
        public int Value { get; }

        public Worker(int value)
        {
            Value = value;
        }

        public void Run()
        {
            Common();
            var record = new AuditRecord(1);
        }
    }

    public enum Status
    {
        Ready,
        Done
    }
}
