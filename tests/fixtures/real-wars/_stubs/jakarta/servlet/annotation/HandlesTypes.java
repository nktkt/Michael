package jakarta.servlet.annotation;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Fixture-only stub of {@code jakarta.servlet.annotation.HandlesTypes}.
 *
 * <p>See {@code jakarta.servlet.ServletContainerInitializer} in the same
 * stubs tree for the broader rationale.
 */
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface HandlesTypes {
    Class<?>[] value();
}
